use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};
use std::sync::Arc;
use std::time::Duration;

/// Optimized SQLite connection with prepared statement cache and WAL mode.
/// Uses a single connection with statement caching for thread safety.
pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Item {
    pub id: String,
    pub url: String,
    pub title: String,
    pub from_playlist: bool,
    pub status: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct HistoryEntry {
    pub id: String,
    pub title: String,
    pub status: String,
    pub from_playlist: bool,
    pub finished_at: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SongRow {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub itag: Option<String>,
    pub abr: Option<f64>,
    pub delivered_kbps: Option<f64>,
    pub size_bytes: i64,
    pub path: String,
    pub downloaded_at: String,
    pub sample_rate: Option<i32>,
    pub channels: Option<i32>,
    pub bit_depth: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct DbConfig {
    pub path: String,
    pub busy_timeout_ms: u32,
    pub wal_mode: bool,
    pub synchronous: SynchronousMode,
    pub cache_size_kib: i32,
    pub mmap_size_mib: usize,
    pub page_size: u32,
    pub temp_store: TempStore,
}

#[derive(Clone, Copy, Debug)]
pub enum SynchronousMode {
    Off = 0,
    Normal = 1,
    Full = 2,
}

#[derive(Clone, Copy, Debug)]
pub enum TempStore {
    Default = 0,
    File = 1,
    Memory = 2,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            path: "queue.db".to_string(),
            busy_timeout_ms: 5000,
            wal_mode: true,
            synchronous: SynchronousMode::Normal,
            cache_size_kib: -32768,
            mmap_size_mib: 256,
            page_size: 4096,
            temp_store: TempStore::Memory,
        }
    }
}

impl Db {
    pub fn new(config: DbConfig) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        
        let conn = Connection::open_with_flags(&config.path, flags)
            .context("opening database")?;
        
        let pragmas = format!(
            "PRAGMA journal_mode={};
             PRAGMA busy_timeout={};
             PRAGMA synchronous={};
             PRAGMA cache_size={};
             PRAGMA mmap_size={};
             PRAGMA page_size={};
             PRAGMA temp_store={};
             PRAGMA optimize;",
            if config.wal_mode { "WAL" } else { "DELETE" },
            config.busy_timeout_ms,
            config.synchronous as i32,
            config.cache_size_kib,
            (config.mmap_size_mib * 1024 * 1024) as i64,
            config.page_size,
            config.temp_store as i32,
        );
        
        conn.execute_batch(&pragmas).context("setting pragmas")?;
        
        let db = Self {
            conn: Mutex::new(conn),
        };
        
        db.init_schema()?;
        Ok(db)
    }
    
    pub fn init(path: &str) -> Result<Self> {
        let config = DbConfig {
            path: path.to_string(),
            ..Default::default()
        };
        Self::new(config)
    }
    
    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (
                id            TEXT PRIMARY KEY,
                pos           INTEGER NOT NULL,
                url           TEXT NOT NULL,
                title         TEXT NOT NULL,
                from_playlist INTEGER NOT NULL DEFAULT 0,
                status        TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_items_pos ON items(pos);
            CREATE INDEX IF NOT EXISTS idx_items_status ON items(status);
            
            CREATE TABLE IF NOT EXISTS history (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                title         TEXT NOT NULL,
                status        TEXT NOT NULL,
                from_playlist INTEGER NOT NULL DEFAULT 0,
                finished_at   TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_history_finished ON history(finished_at DESC);
            
            CREATE TABLE IF NOT EXISTS songs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                video_id      TEXT NOT NULL,
                title         TEXT NOT NULL,
                artist        TEXT NOT NULL,
                itag          TEXT,
                abr           REAL,
                delivered_kbps REAL,
                size_bytes    INTEGER NOT NULL,
                path          TEXT NOT NULL,
                downloaded_at TEXT NOT NULL,
                sample_rate   INTEGER,
                channels      INTEGER,
                bit_depth     INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_songs_video_id ON songs(video_id);
            CREATE INDEX IF NOT EXISTS idx_songs_artist ON songs(artist);
            CREATE INDEX IF NOT EXISTS idx_songs_downloaded ON songs(downloaded_at DESC);
            
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            
            CREATE TABLE IF NOT EXISTS download_queue (
                id            TEXT PRIMARY KEY,
                url           TEXT NOT NULL,
                title         TEXT NOT NULL,
                artist        TEXT,
                priority      INTEGER NOT NULL DEFAULT 0,
                status        TEXT NOT NULL DEFAULT 'pending',
                estimated_size INTEGER NOT NULL DEFAULT 0,
                retry_count   INTEGER NOT NULL DEFAULT 0,
                created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                started_at    TEXT,
                completed_at  TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_queue_priority ON download_queue(priority DESC, created_at);
            CREATE INDEX IF NOT EXISTS idx_queue_status ON download_queue(status);
            
            CREATE TABLE IF NOT EXISTS song_fingerprints (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                video_id      TEXT NOT NULL UNIQUE,
                title_hash    TEXT NOT NULL,
                artist_hash   TEXT NOT NULL,
                duration      REAL,
                prefix_hash   TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_fingerprints_prefix ON song_fingerprints(prefix_hash);",
        ).context("schema init")?;
        Ok(())
    }
    
    pub fn load_items(&self) -> Result<Vec<Item>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, url, title, from_playlist, status FROM items ORDER BY pos")?;
        let rows = stmt.query_map([], |row| {
            Ok(Item {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                from_playlist: row.get::<_, i64>(3)? != 0,
                status: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }
    
    pub fn persist_items(&self, order: &[String], items: &rustc_hash::FxHashMap<String, Item>) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM items", [])?;
        for (pos, id) in order.iter().enumerate() {
            if let Some(item) = items.get(id) {
                tx.execute(
                    "INSERT INTO items (id, pos, url, title, from_playlist, status) VALUES (?, ?, ?, ?, ?, ?)",
                    rusqlite::params![item.id, pos as i64, item.url, item.title, item.from_playlist as i64, item.status],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    
    pub fn insert_history(&self, entry: &HistoryEntry, limit: usize) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO history (title, status, from_playlist, finished_at) VALUES (?, ?, ?, ?)",
            rusqlite::params![entry.title, entry.status, entry.from_playlist as i64, entry.finished_at],
        )?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))?;
        if count > limit as i64 {
            conn.execute(
                "DELETE FROM history WHERE id IN (SELECT id FROM history ORDER BY id LIMIT ?)",
                [count - limit as i64],
            )?;
        }
        Ok(())
    }
    
    pub fn load_history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, title, status, from_playlist, finished_at FROM history ORDER BY id DESC LIMIT ?")?;
        let rows = stmt.query_map([limit], |row| Ok(HistoryEntry {
            id: row.get::<_, i64>(0)?.to_string(),
            title: row.get(1)?,
            status: row.get(2)?,
            from_playlist: row.get::<_, i64>(3)? != 0,
            finished_at: row.get(4)?,
        }))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }
    
    pub fn get_setting(&self, key: &str, default: i64) -> i64 {
        let conn = self.conn.lock();
        conn.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    
    pub fn save_setting(&self, key: &str, value: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value.to_string()],
        )?;
        Ok(())
    }
    
    pub fn insert_song(&self, song: &SongRow) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO songs (video_id, title, artist, itag, abr, delivered_kbps, size_bytes, path, downloaded_at, sample_rate, channels, bit_depth)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                song.video_id, song.title, song.artist, song.itag,
                song.abr, song.delivered_kbps, song.size_bytes,
                song.path, song.downloaded_at, song.sample_rate,
                song.channels, song.bit_depth
            ],
        )?;
        Ok(())
    }
    
    pub fn list_songs(&self) -> Result<(Vec<SongRow>, i64, i64)> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT video_id, title, artist, itag, abr, delivered_kbps, size_bytes, path, downloaded_at, sample_rate, channels, bit_depth
             FROM songs ORDER BY id DESC"
        )?;
        let rows = stmt.query_map([], |row| Ok(SongRow {
            video_id: row.get(0)?,
            title: row.get(1)?,
            artist: row.get(2)?,
            itag: row.get(3)?,
            abr: row.get(4)?,
            delivered_kbps: row.get(5)?,
            size_bytes: row.get(6)?,
            path: row.get(7)?,
            downloaded_at: row.get(8)?,
            sample_rate: row.get(9)?,
            channels: row.get(10)?,
            bit_depth: row.get(11)?,
        }))?;
        let all: Vec<SongRow> = rows.filter_map(Result::ok).collect();
        let total_bytes: i64 = all.iter().map(|s| s.size_bytes).sum();
        let total: i64 = all.len() as i64;
        Ok((all, total, total_bytes))
    }
    
    pub fn add_to_queue(&self, item: &crate::queue_organizer::QueueItem) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO download_queue (id, url, title, artist, priority, estimated_size, status)
             VALUES (?, ?, ?, ?, ?, ?, 'pending')",
            rusqlite::params![
                item.id, item.url, item.title, item.artist,
                item.priority, item.estimated_size
            ],
        )?;
        Ok(())
    }
    
    pub fn get_next_priority(&self) -> Result<Option<crate::queue_organizer::QueueItem>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, url, title, artist, priority, status, estimated_size, retry_count, created_at, started_at, completed_at
             FROM download_queue
             WHERE status = 'pending'
             ORDER BY priority DESC, created_at ASC
             LIMIT 1"
        )?;
        
        let item = stmt.query_row([], |row| Ok(crate::queue_organizer::QueueItem {
            id: row.get(0)?,
            url: row.get(1)?,
            title: row.get(2)?,
            artist: row.get(3)?,
            priority: row.get(4)?,
            status: row.get(5)?,
            estimated_size: row.get(6)?,
            actual_size: row.get(7)?,
            retry_count: row.get(8)?,
            created_at: row.get(9)?,
            started_at: row.get(10)?,
            completed_at: row.get(11)?,
            quality_score: row.get(12)?,
            metadata: crate::queue_organizer::ItemMetadata::default(),
        })).ok();
        
        if let Some(ref item) = item {
            conn.execute(
                "UPDATE download_queue SET status = 'downloading', started_at = datetime('now') WHERE id = ?",
                [&item.id],
            )?;
        }
        
        Ok(item)
    }
    
    pub fn update_queue_status(&self, id: &str, status: &str, retry_count: Option<i32>) -> Result<()> {
        let conn = self.conn.lock();
        if let Some(rc) = retry_count {
            conn.execute(
                "UPDATE download_queue SET status = ?, retry_count = ? WHERE id = ?",
                rusqlite::params![status, rc, id],
            )?;
        } else {
            conn.execute(
                "UPDATE download_queue SET status = ?, completed_at = datetime('now') WHERE id = ?",
                rusqlite::params![status, id],
            )?;
        }
        Ok(())
    }
    
    pub fn get_queue(&self, limit: usize) -> Result<Vec<crate::queue_organizer::QueueItem>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, url, title, artist, priority, status, estimated_size, retry_count, created_at, started_at, completed_at
             FROM download_queue
             ORDER BY priority DESC, created_at ASC
             LIMIT ?"
        )?;
        
        let rows = stmt.query_map([limit], |row| Ok(crate::queue_organizer::QueueItem {
            id: row.get(0)?,
            url: row.get(1)?,
            title: row.get(2)?,
            artist: row.get(3)?,
            priority: row.get(4)?,
            status: row.get(5)?,
            estimated_size: row.get(6)?,
            actual_size: row.get(7)?,
            retry_count: row.get(8)?,
            created_at: row.get(9)?,
            started_at: row.get(10)?,
            completed_at: row.get(11)?,
            quality_score: row.get(12)?,
            metadata: crate::queue_organizer::ItemMetadata::default(),
        }))?;
        
        Ok(rows.filter_map(Result::ok).collect::<Vec<_>>())
    }
    
    pub fn insert_fingerprint(&self, fp: &crate::queue_organizer::SongFingerprint) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO song_fingerprints (video_id, title_hash, artist_hash, duration, prefix_hash)
             VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![fp.video_id, fp.title_hash, fp.artist_hash, fp.duration, fp.prefix_hash],
        )?;
        Ok(())
    }
    
    pub fn find_similar_by_prefix(&self, prefix_hash: &str, limit: usize) -> Result<Vec<crate::queue_organizer::SongFingerprint>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT video_id, title_hash, artist_hash, duration, prefix_hash
             FROM song_fingerprints
             WHERE prefix_hash LIKE ?
             ORDER BY prefix_hash DESC
             LIMIT ?"
        )?;
        
        let like_pattern = format!("{}%", prefix_hash);
        let rows = stmt.query_map(rusqlite::params![like_pattern, limit], |row| Ok(crate::queue_organizer::SongFingerprint {
            video_id: row.get(0)?,
            title_hash: row.get(1)?,
            artist_hash: row.get(2)?,
            duration: row.get(3)?,
            prefix_hash: row.get(4)?,
        }))?;
        
        Ok(rows.filter_map(Result::ok).collect::<Vec<_>>())
    }
}