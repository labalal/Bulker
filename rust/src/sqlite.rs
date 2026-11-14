use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
use std::sync::Arc;

/// Minimal SQLite wrapper with connection pooling, deadlock prevention,
/// and prepared pragmas optimized for 8KB pages and NVMe/SSD storage.
pub struct Sqlite {
    pool: Arc<Mutex<Vec<Connection>>>,
    pub path: String,
    config: SqliteConfig,
}

#[derive(Clone, Debug)]
pub struct SqliteConfig {
    pub path: String,
    pub pool_size: usize,
    pub busy_timeout_ms: u32,
    pub wal_mode: bool,
    pub cache_size_kib: i32,
    pub mmap_size_mib: usize,
    pub page_size: u32,
    pub temp_store: TempStore,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum TempStore {
    #[default]
    Default = 0,
    File = 1,
    Memory = 2,
}

const SQLITE_PAGE_SIZE: usize = 8192;

impl Default for SqliteConfig {
    fn default() -> Self {
        Self {
            path: "queue.db".to_string(),
            pool_size: 6,
            busy_timeout_ms: 10000, // 10s busy timeout to prevent SQLITE_BUSY deadlocks
            wal_mode: true,
            cache_size_kib: -65536, // 64 MB memory cache
            mmap_size_mib: 256,     // 256 MB memory-mapped I/O
            page_size: SQLITE_PAGE_SIZE as u32, // 8192 (8KB pages)
            temp_store: TempStore::Memory,
        }
    }
}

impl Sqlite {
    pub fn new(config: SqliteConfig) -> Result<Self> {
        let mut pool = Vec::with_capacity(config.pool_size);
        
        for _ in 0..config.pool_size {
            let conn = Self::create_conn(&config)?;
            pool.push(conn);
        }
        
        let db = Self {
            pool: Arc::new(Mutex::new(pool)),
            path: config.path.clone(),
            config,
        };
        
        db.init_schema()?;
        Ok(db)
    }
    
    fn create_conn(config: &SqliteConfig) -> Result<Connection> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        
        let conn = Connection::open_with_flags(&config.path, flags)
            .context("opening database")?;
        
        // Highly tuned pragmas:
        // - 8KB page size aligns with SSD clusters and reduces B-Tree depth
        // - WAL mode allows concurrent readers while writing
        // - NORMAL sync is fully crash-safe in WAL mode while eliminating redundant fsyncs
        // - busy_timeout prevents immediate deadlocks on lock contention
        let pragmas = format!(
            "PRAGMA page_size={};
             PRAGMA journal_mode={};
             PRAGMA busy_timeout={};
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size={};
             PRAGMA mmap_size={};
             PRAGMA temp_store={};
             PRAGMA optimize;",
            config.page_size,
            if config.wal_mode { "WAL" } else { "DELETE" },
            config.busy_timeout_ms,
            config.cache_size_kib,
            (config.mmap_size_mib * 1024 * 1024) as i64,
            config.temp_store as i32,
        );
        
        conn.execute_batch(&pragmas).context("setting pragmas")?;
        Ok(conn)
    }
    
    fn init_schema(&self) -> Result<()> {
        let mut conn = self.get_conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (
                id TEXT PRIMARY KEY,
                pos INTEGER NOT NULL,
                url TEXT NOT NULL,
                title TEXT NOT NULL,
                from_playlist INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_items_pos ON items(pos);
            
            CREATE TABLE IF NOT EXISTS history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                from_playlist INTEGER NOT NULL DEFAULT 0,
                finished_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_history_finished_at ON history(finished_at DESC);
            
            CREATE TABLE IF NOT EXISTS songs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                video_id TEXT NOT NULL,
                title TEXT NOT NULL,
                artist TEXT NOT NULL,
                itag TEXT,
                abr REAL,
                delivered_kbps REAL,
                size_bytes INTEGER NOT NULL,
                path TEXT NOT NULL,
                downloaded_at TEXT NOT NULL,
                sample_rate INTEGER,
                channels INTEGER,
                bit_depth INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_songs_video_id ON songs(video_id);
            CREATE INDEX IF NOT EXISTS idx_songs_artist ON songs(artist);
            
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            
            CREATE TABLE IF NOT EXISTS download_queue (
                id TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                title TEXT NOT NULL,
                artist TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'pending',
                estimated_size INTEGER NOT NULL DEFAULT 0,
                actual_size INTEGER,
                retry_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                started_at TEXT,
                completed_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_queue_status_priority ON download_queue(status, priority DESC, created_at ASC);
            
            CREATE TABLE IF NOT EXISTS song_fingerprints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                video_id TEXT NOT NULL UNIQUE,
                title_hash TEXT NOT NULL,
                artist_hash TEXT NOT NULL,
                duration REAL,
                prefix_hash TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_fingerprints_prefix ON song_fingerprints(prefix_hash, video_id);",
        )?;
        Ok(())
    }
    
    pub fn get_conn(&self) -> Result<PooledConn> {
        let mut pool = self.pool.lock();
        if let Some(conn) = pool.pop() {
            Ok(PooledConn { conn: Some(conn), pool: self.pool.clone() })
        } else {
            let conn = Self::create_conn(&self.config)?;
            Ok(PooledConn { conn: Some(conn), pool: self.pool.clone() })
        }
    }
    
    pub fn exec(&self, sql: &str) -> Result<()> {
        let mut conn = self.get_conn()?;
        conn.execute_batch(sql)?;
        Ok(())
    }
    
    /// Executes a write transaction with BEGIN IMMEDIATE to prevent deadlock.
    pub fn immediate_transaction<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction) -> Result<T>,
    {
        let mut conn = self.get_conn()?;
        let tx = conn.raw_conn_mut().transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to start immediate transaction")?;
        let result = f(&tx)?;
        tx.commit().context("failed to commit transaction")?;
        Ok(result)
    }
}

pub struct PooledConn {
    conn: Option<Connection>,
    pool: Arc<Mutex<Vec<Connection>>>,
}

impl PooledConn {
    pub fn raw_conn_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("valid pooled connection")
    }

    pub fn execute_batch(&mut self, sql: &str) -> Result<()> {
        self.raw_conn_mut().execute_batch(sql).context("executing SQL batch")?;
        Ok(())
    }
    
    pub fn query_row<T, F>(&mut self, sql: &str, params: &[&dyn rusqlite::ToSql], f: F) -> Result<Option<T>>
    where
        F: FnOnce(&rusqlite::Row) -> rusqlite::Result<T>,
    {
        self.raw_conn_mut().query_row(sql, params, f)
            .optional()
            .context("executing query_row")
    }
    
    pub fn query<T, F>(&mut self, sql: &str, params: &[&dyn rusqlite::ToSql], f: F) -> Result<Vec<T>>
    where
        F: FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
    {
        let mut stmt = self.raw_conn_mut().prepare(sql).context("preparing statement")?;
        let rows = stmt.query_map(params, f).context("query_map failed")?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("reading row")?);
        }
        Ok(results)
    }
    
    pub fn execute(&mut self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<usize> {
        self.raw_conn_mut().execute(sql, params).context("executing SQL")
    }
    
    /// Starts a transaction with BEGIN IMMEDIATE to guarantee no upgrade deadlocks.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.raw_conn_mut().transaction_with_behavior(TransactionBehavior::Immediate)
            .context("starting immediate transaction")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.lock().push(conn);
        }
    }
}
