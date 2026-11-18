use crate::sqlite::{Sqlite, SqliteConfig, PooledConn};
use anyhow::Result;
use rustc_hash::FxHashMap;

/// Database wrapper using minimal Sqlite connection pool and batched transactions.
pub struct Db {
    pub sqlite: Sqlite,
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

pub type DbConfig = SqliteConfig;

impl Db {
    pub fn new(config: DbConfig) -> Result<Self> {
        Ok(Self { sqlite: Sqlite::new(config)? })
    }
    
    pub fn init(path: &str) -> Result<Self> {
        Self::new(DbConfig { path: path.to_string(), ..Default::default() })
    }
    
    fn conn(&self) -> Result<PooledConn> {
        self.sqlite.get_conn()
    }
    
    pub fn load_items(&self) -> Result<Vec<Item>> {
        self.conn()?.query(
            "SELECT id, url, title, from_playlist, status FROM items ORDER BY pos",
            &[],
            |row| Ok(Item {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                from_playlist: row.get::<_, i64>(3)? != 0,
                status: row.get(4)?,
            }),
        )
    }
    
    /// Persists items in a single atomic IMMEDIATE transaction, preventing deadlock.
    pub fn persist_items(&self, order: &[String], items: &FxHashMap<String, Item>) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM items", [])?;
        for (pos, id) in order.iter().enumerate() {
            if let Some(item) = items.get(id) {
                tx.execute(
                    "INSERT OR REPLACE INTO items (id, pos, url, title, from_playlist, status) VALUES (?, ?, ?, ?, ?, ?)",
                    rusqlite::params![item.id, pos as i64, item.url, item.title, item.from_playlist as i64, item.status],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    
    pub fn insert_history(&self, entry: &HistoryEntry, limit: usize) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO history (title, status, from_playlist, finished_at) VALUES (?, ?, ?, ?)",
            rusqlite::params![entry.title, entry.status, entry.from_playlist as i64, entry.finished_at],
        )?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))?;
        if count > limit as i64 {
            tx.execute(
                "DELETE FROM history WHERE id IN (SELECT id FROM history ORDER BY id LIMIT ?)",
                rusqlite::params![count - limit as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    
    pub fn load_history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.conn()?.query(
            "SELECT id, title, status, from_playlist, finished_at FROM history ORDER BY id DESC LIMIT ?",
            &[&limit],
            |row| Ok(HistoryEntry {
                id: row.get::<_, i64>(0)?.to_string(),
                title: row.get(1)?,
                status: row.get(2)?,
                from_playlist: row.get::<_, i64>(3)? != 0,
                finished_at: row.get(4)?,
            }),
        )
    }
    
    pub fn get_setting(&self, key: &str, default: i64) -> i64 {
        self.conn()
            .and_then(|mut c| {
                c.query_row("SELECT value FROM settings WHERE key = ?", &[&key], |r| r.get::<_, String>(0))
            })
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    
    pub fn save_setting(&self, key: &str, value: i64) -> Result<()> {
        let mut conn = self.conn()?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            &[&key, &value.to_string() as &dyn rusqlite::ToSql],
        )?;
        Ok(())
    }
    
    pub fn insert_song(&self, song: &SongRow) -> Result<()> {
        let mut conn = self.conn()?;
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

    /// Batched insert of multiple songs in a single transaction
    pub fn batch_insert_songs(&self, songs: &[SongRow]) -> Result<()> {
        if songs.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO songs (video_id, title, artist, itag, abr, delivered_kbps, size_bytes, path, downloaded_at, sample_rate, channels, bit_depth)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            )?;
            for song in songs {
                stmt.execute(rusqlite::params![
                    song.video_id, song.title, song.artist, song.itag,
                    song.abr, song.delivered_kbps, song.size_bytes,
                    song.path, song.downloaded_at, song.sample_rate,
                    song.channels, song.bit_depth
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    
    pub fn list_songs(&self) -> Result<(Vec<SongRow>, i64, i64)> {
        let rows = self.conn()?.query(
            "SELECT video_id, title, artist, itag, abr, delivered_kbps, size_bytes, path, downloaded_at, sample_rate, channels, bit_depth
             FROM songs ORDER BY id DESC",
            &[],
            |row| Ok(SongRow {
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
            }),
        )?;
        let total_bytes: i64 = rows.iter().map(|s| s.size_bytes).sum();
        let total: i64 = rows.len() as i64;
        Ok((rows, total, total_bytes))
    }
    
    pub fn add_to_queue(&self, item: &crate::queue_organizer::QueueItem) -> Result<()> {
        let mut conn = self.conn()?;
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

    /// Batched insert of queue items in a single immediate transaction.
    /// Drastically reduces disk writes and eliminates lock contention.
    pub fn batch_add_to_queue(&self, items: &[crate::queue_organizer::QueueItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO download_queue (id, url, title, artist, priority, estimated_size, status)
                 VALUES (?, ?, ?, ?, ?, ?, 'pending')"
            )?;
            for item in items {
                stmt.execute(rusqlite::params![
                    item.id, item.url, item.title, item.artist,
                    item.priority, item.estimated_size
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    
    pub fn get_next_priority(&self) -> Result<Option<crate::queue_organizer::QueueItem>> {
        let mut conn = self.conn()?;
        conn.query_row(
            "SELECT id, url, title, artist, priority, status, estimated_size, actual_size, retry_count, created_at, started_at, completed_at
             FROM download_queue
             WHERE status = 'pending'
             ORDER BY priority DESC, created_at ASC
             LIMIT 1",
            &[],
            |row| {
                Ok(crate::queue_organizer::QueueItem {
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
                    quality_score: None,
                    metadata: crate::queue_organizer::ItemMetadata::default(),
                })
            },
        )
    }
    
    /// Load all queue items from download_queue table (for startup restoration)
    pub fn load_all_queue_items(&self) -> Result<Vec<crate::queue_organizer::QueueItem>> {
        self.conn()?.query(
            "SELECT id, url, title, artist, priority, status, estimated_size, actual_size, retry_count, created_at, started_at, completed_at
             FROM download_queue
             WHERE status IN ('pending', 'scheduled', 'Starting...')
             ORDER BY priority DESC, created_at ASC",
            &[],
            |row| {
                Ok(crate::queue_organizer::QueueItem {
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
                    quality_score: None,
                    metadata: crate::queue_organizer::ItemMetadata::default(),
                })
            },
        )
    }
    
    pub fn update_queue_status(&self, id: &str, status: &str, retry_count: Option<i32>) -> Result<()> {
        let mut conn = self.conn()?;
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

    /// Batched status updates in a single immediate transaction
    pub fn batch_update_queue_status(&self, updates: &[(String, String, Option<i32>)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt_retry = tx.prepare_cached(
                "UPDATE download_queue SET status = ?, retry_count = ? WHERE id = ?"
            )?;
            let mut stmt_done = tx.prepare_cached(
                "UPDATE download_queue SET status = ?, completed_at = datetime('now') WHERE id = ?"
            )?;
            for (id, status, retry) in updates {
                if let Some(rc) = retry {
                    stmt_retry.execute(rusqlite::params![status, rc, id])?;
                } else {
                    stmt_done.execute(rusqlite::params![status, id])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
    
    pub fn insert_fingerprint(&self, fp: &crate::queue_organizer::SongFingerprint) -> Result<()> {
        let mut conn = self.conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO song_fingerprints (video_id, title_hash, artist_hash, duration, prefix_hash)
             VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![fp.video_id, fp.title_hash, fp.artist_hash, fp.duration, fp.prefix_hash],
        )?;
        Ok(())
    }

    /// Batched insert of song fingerprints in a single transaction
    pub fn batch_insert_fingerprints(&self, fps: &[crate::queue_organizer::SongFingerprint]) -> Result<()> {
        if fps.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO song_fingerprints (video_id, title_hash, artist_hash, duration, prefix_hash)
                 VALUES (?, ?, ?, ?, ?)"
            )?;
            for fp in fps {
                stmt.execute(rusqlite::params![
                    fp.video_id, fp.title_hash, fp.artist_hash, fp.duration, fp.prefix_hash
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    
    pub fn find_similar_by_prefix(&self, prefix_hash: &str, limit: usize) -> Result<Vec<crate::queue_organizer::SongFingerprint>> {
        let mut conn = self.conn()?;
        let like_pattern = format!("{}%", prefix_hash);
        conn.query(
            "SELECT video_id, title_hash, artist_hash, duration, prefix_hash
             FROM song_fingerprints
             WHERE prefix_hash LIKE ?
             ORDER BY prefix_hash DESC
             LIMIT ?",
            &[&like_pattern as &dyn rusqlite::ToSql, &limit as &dyn rusqlite::ToSql],
            |row| Ok(crate::queue_organizer::SongFingerprint {
                video_id: row.get(0)?,
                title_hash: row.get(1)?,
                artist_hash: row.get(2)?,
                duration: row.get(3)?,
                prefix_hash: row.get(4)?,
            }),
        )
    }
}
