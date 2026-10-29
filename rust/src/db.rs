use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Mutex;

/// Thin storage layer backed by plain SQLite (replaces the Python app's
/// queue_store.json). Three tables: queue items in playback order, a
/// finished-items history, and runtime settings. A single Mutex<Connection>
/// is plenty: every mutation is a handful of statements on a small DB.
pub struct Db(pub Mutex<Connection>);

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

pub fn init(path: &str) -> Result<Db> {
    let conn = Connection::open(path).context("opening queue.db")?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         CREATE TABLE IF NOT EXISTS items (
            id            TEXT PRIMARY KEY,
            pos           INTEGER NOT NULL,
            url           TEXT NOT NULL,
            title         TEXT NOT NULL,
            from_playlist INTEGER NOT NULL DEFAULT 0,
            status        TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS history (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            title         TEXT NOT NULL,
            status        TEXT NOT NULL,
            from_playlist INTEGER NOT NULL DEFAULT 0,
            finished_at   TEXT NOT NULL
         );
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
            downloaded_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );",
    )
    .context("schema init")?;
    Ok(Db(Mutex::new(conn)))
}

pub fn load_items(db: &Db) -> Result<Vec<Item>> {
    let conn = db.0.lock().unwrap();
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

/// Write-through snapshot of the whole queue (same shape the Python version
/// wrote to queue_store.json, just atomic inside SQLite instead).
pub fn persist_items(db: &Db, order: &[String], items: &HashMap<String, Item>) -> Result<()> {
    let mut conn = db.0.lock().unwrap();
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

pub fn insert_history(db: &Db, entry: &HistoryEntry, limit: usize) -> Result<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "INSERT INTO history (title, status, from_playlist, finished_at) VALUES (?, ?, ?, ?)",
        rusqlite::params![entry.title, entry.status, entry.from_playlist as i64, entry.finished_at],
    )?;
    // Rowid-based trim (the Python version trimmed the in-memory Vec).
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))?;
    if count > limit as i64 {
        conn.execute(
            "DELETE FROM history WHERE id IN (SELECT id FROM history ORDER BY id LIMIT ?)",
            [count - limit as i64],
        )?;
    }
    Ok(())
}

pub fn load_history(db: &Db) -> Result<Vec<HistoryEntry>> {
    let conn = db.0.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, title, status, from_playlist, finished_at FROM history ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(HistoryEntry {
            id: row.get::<_, i64>(0)?.to_string(),
            title: row.get(1)?,
            status: row.get(2)?,
            from_playlist: row.get::<_, i64>(3)? != 0,
            finished_at: row.get(4)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn get_setting(db: &Db, key: &str, default: i64) -> i64 {    let conn = db.0.lock().unwrap();
    conn.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn save_setting(db: &Db, key: &str, value: i64) {
    if let Ok(conn) = db.0.lock() {
        let _ = conn.execute(
            "INSERT INTO settings (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value.to_string()],
        );
    }
}

// --- downloaded-music library ----------------------------------------------

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
}

/// Catalog every completed download: what music you actually have, at what
/// quality, where the file sits and how much of your disk budget is spent.
pub fn insert_song(db: &Db, song: &SongRow) {
    if let Ok(conn) = db.0.lock() {
        let _ = conn.execute(
            "INSERT INTO songs (video_id, title, artist, itag, abr, delivered_kbps,
                                size_bytes, path, downloaded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                song.video_id,
                song.title,
                song.artist,
                song.itag,
                song.abr,
                song.delivered_kbps,
                song.size_bytes,
                song.path,
                song.downloaded_at
            ],
        );
    }
}

pub fn list_songs(db: &Db) -> (Vec<SongRow>, i64, i64) {
    let Ok(conn) = db.0.lock() else {
        return (vec![], 0, 0);
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT video_id, title, artist, itag, abr, delivered_kbps, size_bytes, path, downloaded_at
         FROM songs ORDER BY id DESC",
    ) else {
        return (vec![], 0, 0);
    };
    let rows = stmt
        .query_map([], |row| {
            Ok(SongRow {
                video_id: row.get(0)?,
                title: row.get(1)?,
                artist: row.get(2)?,
                itag: row.get(3)?,
                abr: row.get(4)?,
                delivered_kbps: row.get(5)?,
                size_bytes: row.get(6)?,
                path: row.get(7)?,
                downloaded_at: row.get(8)?,
            })
        })
        .ok();
    let all = rows
        .map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();
    let total_bytes: i64 = all.iter().map(|s| s.size_bytes).sum();
    let total: i64 = all.len() as i64;
    (all, total, total_bytes)
}
