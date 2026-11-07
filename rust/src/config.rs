use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Tiny hand-rolled .env loader (project decided against dotenv crates to
/// keep the dependency list short). Lines of KEY=VALUE are pushed into the
/// process environment if not already set. Values may be quoted, comments
/// (#) allowed.
fn load_dotenv(path: &str) {
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut split = line.splitn(2, '=');
            let (Some(key), Some(value)) = (split.next(), split.next()) else {
                continue;
            };
            let key = key.trim();
            let mut value = value.trim().to_string();
            if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                value = value[1..value.len() - 1].to_string();
            }
            if std::env::var(key).is_err() {
                std::env::set_var(key, value);
            }
        }
    }
}

pub struct Config {
    pub cookies_file: String,
    pub archive_file: String,
    // legacy; rescued into songs_archive at startup
    pub temp_workspace_dir: String,
    pub songs_archive_dir: String,
    pub queue_db_path: String,
    pub log_file: String,
    pub host: String,
    pub port: u16,
    pub aria2c_connections: i64,
    pub concurrency_default: usize,
    pub concurrency_max: usize,
    pub search_result_limit: usize,
    pub auto_remove_delay_seconds: u64,
    pub history_limit: usize,
    pub download_retries: i64,
    pub socket_timeout: i64,
    pub auto_retry_attempts: usize,
    pub auto_retry_backoff_seconds: u64,
    pub default_rate_limit_kibs: i64,
    pub thumbnail_max_dimension: i64,
}

fn getenv(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn getenv_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn get_base_dir() -> PathBuf {
    if let Ok(base) = std::env::var("BULKER_BASE_PATH") {
        if !base.trim().is_empty() {
            return PathBuf::from(base);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Projects").join("Bulker");
    }
    PathBuf::from(".")
}

fn resolve_path(key: &str, default: &str, base: &Path) -> String {
    let value = getenv(key, default);
    let path = Path::new(&value);
    if path.is_absolute() {
        value
    } else {
        base.join(path).to_string_lossy().into_owned()
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn config() -> &'static Config {
    CONFIG.get().expect("config initialized")
}

pub fn init() {
    let base = get_base_dir();
    let dotenv_path = base.join(".env");
    load_dotenv(&dotenv_path.to_string_lossy());
    if let Err(e) = std::fs::create_dir_all(&base) {
        eprintln!(
            "warning: could not create Bulker base directory {}: {e}",
            base.display()
        );
    }

    let cookies_file = resolve_path("COOKIES_FILE", "cookies.txt", &base);
    let archive_file = resolve_path("ARCHIVE_FILE", "archive.txt", &base);
    
    let temp_workspace_dir = resolve_path("TEMP_WORKSPACE_DIR", "temp_workspace", &base);
    let songs_archive_dir = resolve_path("SONGS_ARCHIVE_DIR", "Songs_archive", &base);
    let queue_db_path = resolve_path("QUEUE_DB_PATH", "queue.db", &base);
    let log_file = resolve_path("LOG_FILE", "app.log", &base);

    let _ = CONFIG.set(Config {
        cookies_file,
        archive_file,
        
        temp_workspace_dir,
        songs_archive_dir,
        queue_db_path,
        log_file,
        host: getenv("HOST", "127.0.0.1"),
        port: getenv_num("PORT", 8000),
        aria2c_connections: getenv_num("ARIA2C_CONNECTIONS", 16),
        concurrency_default: getenv_num("DEFAULT_CONCURRENCY", 3),
        concurrency_max: getenv_num("MAX_CONCURRENCY", 6),
        search_result_limit: getenv_num("SEARCH_RESULT_LIMIT", 8),
        auto_remove_delay_seconds: getenv_num("AUTO_REMOVE_DELAY_SECONDS", 3),
        history_limit: getenv_num("HISTORY_LIMIT", 200),
        download_retries: getenv_num("DOWNLOAD_RETRIES", 10),
        socket_timeout: getenv_num("SOCKET_TIMEOUT", 30),
        auto_retry_attempts: getenv_num("AUTO_RETRY_ATTEMPTS", 3),
        auto_retry_backoff_seconds: getenv_num("AUTO_RETRY_BACKOFF_SECONDS", 15),
        default_rate_limit_kibs: getenv_num("RATE_LIMIT_KIBS", 0),
        thumbnail_max_dimension: getenv_num("THUMBNAIL_MAX_DIMENSION", 320),
    });
}

static COOKIES_EXPIRED: AtomicBool = AtomicBool::new(false);

pub fn set_cookies_expired() {
    COOKIES_EXPIRED.store(true, Ordering::Relaxed);
}

pub fn cookies_expired() -> bool {
    COOKIES_EXPIRED.load(Ordering::Relaxed)
}

pub fn acknowledge_cookies_expiry() {
    COOKIES_EXPIRED.store(false, Ordering::Relaxed);
}
