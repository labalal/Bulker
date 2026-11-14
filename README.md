# Bulker — YouTube Music Bulk Downloader

A high-performance, retry-hardened bulk audio downloader for YouTube Music. Built in Rust with Axum, SQLite, and yt-dlp/aria2c/ffmpeg pipeline. Downloads native 256kbps Opus (Premium itag 774) with embedded metadata and organized folder structure.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        BULKER ARCHITECTURE                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐      │
│  │   Web UI     │    │   CLI        │    │   ADB Phone  │      │
│  │  (Axum)      │    │  Commands    │    │   Scanner    │      │
│  └──────┬───────┘    └──────┬───────┘    └──────┬───────┘      │
│         │                   │                   │               │
│         ▼                   ▼                   ▼               │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                    HTTP HANDLERS                          │  │
│  │  /api/add  /api/search  /api/queue  /api/songs  /api/   │  │
│  └──────────────────────────┬───────────────────────────────┘  │
│                             │                                   │
│                             ▼                                   │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                    APP STATE (Arc<Mutex<>>)               │  │
│  │  • items: FxHashMap<id, Item>     • order: Vec<id>        │  │
│  │  • steps: FxHashMap<id, Steps>    • history: Vec<Hist>    │  │
│  │  • cancel_flags: FxHashMap<id, AtomicBool>                │  │
│  │  • settings: Settings              • active: usize        │  │
│  └──────────────────────────┬───────────────────────────────┘  │
│                             │                                   │
│          ┌──────────────────┼──────────────────┐               │
│          ▼                  ▼                  ▼               │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐         │
│  │   WORKER     │  │   PERSIST    │  │   SETTINGS   │         │
│  │   POOL       │  │   (SQLite)   │  │   (SQLite)   │         │
│  │  (tokio)     │  │   queue.db   │  │   settings   │         │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘         │
│         │                 │                 │                   │
│         ▼                 ▼                 ▼                   │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                    DOWNLOAD PIPELINE                       │  │
│  │                                                             │  │
│  │  1. yt-dlp --dump-json (metadata + format list)           │  │
│  │  2. yt-dlp + aria2c (download → temp_workspace)           │  │
│  │  3. ffmpeg (thumbnail compress to 320px JPEG)             │  │
│  │  4. lofty (embed Vorbis comments: title, artist, cover)   │  │
│  │  5. Organizer (Artist/Title [id].opus, Long_Form split)   │  │
│  │  6. Move to Songs_archive/, cleanup temp                   │  │
│  │  7. Record to SQLite library + history                     │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Project Structure

```
Bulker/
├── Cargo.toml                 # Workspace config
├── cookies.txt                # YouTube Premium cookies (Netscape format)
├── .env                       # Runtime overrides (optional)
├── video_ids.txt              # List of video IDs for batch queue
│
├── web/                       # Embedded static web UI
│   └── index.html             # Single-page Vue.js-like UI
│
└── rust/                      # Rust backend
    ├── Cargo.toml
    └── src/
        ├── main.rs            # Entry point, HTTP server, CLI, worker pool
        ├── config.rs          # Configuration (.env + env vars + defaults)
        ├── db.rs              # SQLite layer (queue, history, songs, settings)
        ├── ytdlp.rs           # yt-dlp wrapper (search, metadata, download)
        ├── tag.rs             # Tagging pipeline (ffmpeg + lofty + organizer)
        ├── organizer.rs       # Folder structure + artist canonicalization
        ├── adb_scanner.rs     # ADB phone music scanner
        ├── tui.rs             # Terminal UI for archive management
        └── log.rs             # Structured logging
```

---

## Core Modules — Public API

### `config.rs` — Configuration System

```rust
pub struct Config {
    pub cookies_file: String,           // Path to cookies.txt
    pub archive_file: String,           // yt-dlp archive file
    pub temp_workspace_dir: String,     // Temporary download dir
    pub songs_archive_dir: String,      // Final organized archive
    pub queue_db_path: String,          // SQLite database path
    pub log_file: String,               // Log file path
    pub host: String,                   // Server bind address
    pub port: u16,                      // Server port
    pub aria2c_connections: i64,        // aria2c split connections
    pub concurrency_default: usize,     // Default concurrent downloads
    pub concurrency_max: usize,         // Max concurrent downloads
    pub search_result_limit: usize,     // Search results per query
    pub auto_remove_delay_seconds: u64, // Auto-remove completed items
    pub history_limit: usize,           // Max history entries
    pub download_retries: i64,          // yt-dlp retries
    pub socket_timeout: i64,            // Network timeout
    pub auto_retry_attempts: usize,     // Worker retry attempts
    pub auto_retry_backoff_seconds: u64,// Retry backoff base
    pub default_rate_limit_kibs: i64,   // Global rate limit (KiB/s)
    pub thumbnail_max_dimension: i64,   // Thumbnail max px
}

pub fn init()                    // Loads .env, env vars, sets defaults
pub fn config() -> &'static Config  // Global config accessor
pub fn set_cookies_expired()     // Marks cookies as expired
pub fn cookies_expired() -> bool // Checks cookie expiry flag
pub fn acknowledge_cookies_expiry() // Clears expiry flag
```

**Priority**: `.env` → `ENV` → built-in defaults. Relative paths resolve under `$BULKER_BASE` or `$HOME/Projects/Bulker`.

---

### `db.rs` — SQLite Storage Layer

```rust
pub struct Db(pub Mutex<Connection>);

pub fn init(path: &str) -> Result<Db>  // Opens DB, creates schema (WAL mode)

pub fn load_items(db: &Db) -> Result<Vec<Item>>
pub fn persist_items(db: &Db, order: &[String], items: &FxHashMap<String, Item>) -> Result<()>

pub fn insert_history(db: &Db, entry: &HistoryEntry, limit: usize) -> Result<()>
pub fn load_history(db: &Db) -> Result<Vec<HistoryEntry>>

pub fn get_setting(db: &Db, key: &str, default: i64) -> i64
pub fn save_setting(db: &Db, key: &str, value: i64)

pub fn insert_song(db: &Db, song: &SongRow)
pub fn list_songs(db: &Db) -> (Vec<SongRow>, i64, i64)  // (songs, count, total_bytes)
```

**Schema** (3 tables + settings):
```sql
CREATE TABLE items (
    id TEXT PRIMARY KEY, pos INTEGER, url TEXT, title TEXT,
    from_playlist INTEGER, status TEXT
);

CREATE TABLE history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT, status TEXT, from_playlist INTEGER, finished_at TEXT
);

CREATE TABLE songs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    video_id TEXT, title TEXT, artist TEXT, itag TEXT,
    abr REAL, delivered_kbps REAL, size_bytes INTEGER,
    path TEXT, downloaded_at TEXT
);

CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);
```

**Optimizations**: WAL journal mode, busy_timeout=5000ms, single `Mutex<Connection>` — all mutations are fast (few rows), no connection pool needed.

---

### `ytdlp.rs` — YouTube Music Interface

```rust
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub title: String, pub url: String, pub uploader: String,
    pub duration: Option<f64>, pub from_playlist: bool,
}

pub fn to_music_youtube_url(url: &str) -> String
    // Rewrites youtube.com/youtu.be/m.youtube.com → music.youtube.com
    // Critical: Only music.youtube.com exposes Premium itag 774 (256kbps Opus)

pub fn parse_input_urls(raw: &str) -> Vec<String>
    // Strips tracking params (si, ab_channel, pp, feature), rewrites to Music URLs

pub async fn list_entries(url: &str) -> Vec<Entry>
    // Expands playlists via --flat-playlist, returns one Entry per video

pub async fn search_videos(query: &str, limit: usize) -> Vec<Entry>
    // Searches music.youtube.com/search?q= (not generic ytsearch:)
    // Ensures results have Music-catalog itags (774, 251, etc.)

pub async fn fetch_metadata(url: &str) -> MetaPass
    // Single --dump-json pass: extracts title, artist, duration,
    // inventories all Opus formats, detects Premium (itag 774, ≥250kbps)

pub async fn check_quality(url: &str) -> serde_json::Value
    // CLI + /api/quality: proves Premium 256kbps availability

#[derive(Debug)]
pub struct DownloadParams { url, aria2_connections, rate_limit_kibs, cancel }

#[derive(Debug)]
pub struct DownloadMeta { opus_path, thumb_path, title, artist, duration,
    premium_opus_available, best_opus_kbps, selected_format_id }

pub async fn download_once(params, msg_tx, painter) -> Result<DownloadMeta, String>
    // Full pipeline: metadata pre-pass → yt-dlp+aria2c download → locate outputs
    // Streams progress via msg_tx ("pct|42.3")
    // Returns DownloadMeta for tagging pipeline
```

**Format Selection**: `bestaudio[format_id=774]/bestaudio[acodec^=opus]/bestaudio/best`
- Tries Premium 256kbps Opus (itag 774) first
- Falls back to best native Opus available to cookie session
- Final fallback: any best audio (avoids pretending lower quality is 256kbps)

---

### `organizer.rs` — Folder Structure & Artist Canonicalization

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicMetadata {
    pub title: String,
    pub artist: String,
    pub duration: Option<f64>,
    pub is_long_form: bool,      // ≥30 minutes
    pub video_id: String,
    pub source_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizedPath {
    pub relative_path: PathBuf,
    pub full_path: PathBuf,
    pub is_long_form: bool,
    pub artist_folder: String,
}

pub struct MusicOrganizer {
    base_dir: PathBuf,
    long_form_threshold_secs: f64,  // Default 1800s (30 min)
    use_musicbrainz: bool,
    // caches
}

impl MusicOrganizer {
    pub fn new(base_dir: PathBuf) -> Self
    pub fn with_long_form_threshold(self, minutes: f64) -> Self
    pub fn with_musicbrainz(self, enabled: bool) -> Self

    pub fn organize(&self, meta: &MusicMetadata) -> Result<OrganizedPath>
        // Returns: Artist/Title [id].opus  OR  Long_Form/Artist/Title [id].opus

    fn canonicalize_artist(&self, raw: &str) -> Result<String>
        // 1. Clean suffixes (Official, VEVO, - Topic, feat., remix, etc.)
        // 2. Check alias cache
        // 3. Query MusicBrainz (ws/2/artist/?query=artist:"name")
        // 4. Fuzzy match against known artists
        // 5. Return cleaned name

    fn build_filename(&self, meta: &MusicMetadata, _: &str) -> String
        // Format: "Title [video_id].opus"

    fn sanitize_filename(&self, name: &str) -> String
        // Replaces / \ : * ? " < > | and control chars with _

    pub fn register_known_artist(&self, artist: &str)
    pub fn add_alias(&self, raw: &str, canonical: &str)
    pub fn ensure_dirs(&self, path: &Path) -> Result<()>
}

pub fn extract_metadata_from_ytdlp(entry: &Entry, duration: Option<f64>) -> MusicMetadata
    // Parses "Title - Artist" from YouTube title, falls back to uploader
```

**Folder Structure**:
```
Songs_archive/
├── Artist Name/
│   ├── Song Title [abc123].opus
│   └── Another Song [def456].opus
└── Long_Form/
    └── Artist Name/
        ├── Live Concert [xyz789].opus  (≥30 min)
        └── DJ Set [uvw012].opus
```

---

### `tag.rs` — Tagging & Archival Pipeline

```rust
pub type Reporter<'a> = &'a (dyn Fn(&str, &str, &str) + Send + Sync);

pub fn finalize_pipeline(
    opus_path: PathBuf,
    thumb_path: Option<PathBuf>,
    title: &str,
    artist: &str,
    cancel: &Arc<AtomicBool>,
    report: Reporter,
    thumb_dim: i64,
    meta: &MusicMetadata,
) -> Result<PathBuf, String>
    // 1. Compress thumbnail to 320px JPEG (ffmpeg)
    // 2. Embed Vorbis comments via lofty (title, artist, cover art)
    // 3. Organize via MusicOrganizer → Songs_archive/Artist/Title [id].opus
    // 4. Cleanup temp thumbnails
    // Returns final archive path
```

**Opus Tagging** (lofty):
- Native Vorbis comments (no mutagen dependency)
- METADATA_BLOCK_PICTURE base64 for cover art
- Lossless: when source is Opus, yt-dlp stream-copies; `--audio-quality 256` only used for transcoding

---

### `adb_scanner.rs` — Phone Music Scanner

```rust
#[derive(Debug, Clone, Serialize)]
pub struct ScannedSong {
    pub title: String, pub artist: String, pub album: Option<String>,
    pub duration: Option<f64>, pub bitrate: Option<i32>,
    pub file_path: String, pub file_size: i64,
}

pub struct AdbScanner { device_id: Option<String> }

impl AdbScanner {
    pub fn new(device_id: Option<String>) -> Self
    pub fn list_devices() -> Result<Vec<String>>  // adb devices
    pub fn scan_music_directories(&self) -> Result<Vec<ScannedSong>>
        // Scans: /sdcard/Music, /storage/emulated/0/Music,
        //        /storage/sdcard1, /storage/sdcard2, /mnt/media_rw
    pub fn filter_low_bitrate(&self, songs, max_kbps) -> Vec<ScannedSong>

    fn extract_metadata(&self, path) -> Result<ScannedSong>
        // Uses `mediainfo` on device for tags + bitrate
}

pub fn build_search_query(song: &ScannedSong) -> String
    // "Title Artist" for yt-dlp search
```

---

### `tui.rs` — Archive Manager TUI

```rust
#[derive(Debug, Clone)]
pub struct ArchiveItem {
    pub title, artist: String, pub path: PathBuf,
    pub size: u64, pub duration: Option<f64>,
    pub bitrate: Option<u32>, pub selected: bool,
}

pub struct ArchiveTui { items, list_state, filter, show_help }

impl ArchiveTui {
    pub fn new(items: Vec<ArchiveItem>) -> Self
    pub fn run(&mut self) -> Result<Vec<ArchiveItem>>
        // Returns selected items for bulk actions
}

pub fn scan_archive_dir(archive_dir: &PathBuf) -> Result<Vec<ArchiveItem>>
    // Recursively reads .opus files, extracts tags via lofty
```

**Keybindings**:
| Key | Action |
|-----|--------|
| `↑/↓` `j/k` | Navigate |
| `Space`/`Enter` | Toggle selection |
| `a` | Select all (filtered) |
| `n` | Clear all |
| `/` | Filter by title/artist |
| `h`/`F1` | Help |
| `q`/`Esc` | Quit + return selected |

---

### `main.rs` — Server, Workers, CLI

```rust
// AppState — shared across all handlers/workers
struct AppState {
    db: Db,
    items: Mutex<FxHashMap<String, Item>>,  // Active queue
    order: Mutex<Vec<String>>,              // Queue order
    steps: Mutex<FxHashMap<String, Steps>>, // Per-item progress
    history: Mutex<Vec<HistoryEntry>>,
    cancel_flags: Mutex<FxHashMap<String, Arc<AtomicBool>>>,
    settings: Mutex<Settings>,
    active_downloads: Mutex<usize>,
}

// Worker Pool
async fn worker()  // Spawned N times (concurrency_max)
    // Loop: claim_next_item → gate_acquire → process_item → gate_release

async fn process_item(item, cancel)
    // Retry loop (auto_retry_attempts):
    //   fetch_metadata → download_once (streams progress) → finalize_pipeline
    //   On success: record_library + record_history
    //   On failure: backoff, retry unless "unavailable"

// HTTP Handlers (all return Json)
GET  /                    → index.html
POST /api/add             → Add URLs (single/playlist/search)
GET  /api/search?q=       → Search YouTube Music
GET  /api/queue           → Full queue state
DELETE /api/queue         → Clear finished
PATCH /api/queue/reorder  → Reorder queue
DELETE /api/queue/:id     → Cancel or remove
POST /api/queue/:id/retry → Retry failed
POST /api/cleanup         → Cleanup workspace
GET  /api/settings        → Current settings
POST /api/settings        → Update settings
POST /api/cookies/ack     → Dismiss cookie warning
GET  /api/history         → Last 50 history
GET  /api/logs?lines=     → Tail app.log
GET  /api/quality?url=    → Check Premium quality
GET  /api/songs           → Library catalog (songs, total, GiB)

// CLI Commands
bulker-rs check <url>          # Quality check (no server)
bulker-rs songs                # List library (no server)
bulker-rs phone-scan [--device] [--max-bitrate] [--no-dry-run]
bulker-rs phone-list [--device]
bulker-rs queue-from-file <file.txt>
bulker-rs archive-tui
```

---

## Optimizations

### Concurrency & Async
- **Tokio worker pool**: `concurrency_max` workers (default 6), each processes one item
- **Semaphore gate**: `active_downloads` counter limits concurrent yt-dlp processes
- **Channel-based progress**: `mpsc::unbounded_channel` streams percentage from yt-dlp stdout
- **No blocking in async**: All fs/process calls in `tokio::task::spawn_blocking` or async equivalents

### Memory
- **MiMalloc**: Global allocator (`mimalloc::MiMalloc`) — lower fragmentation, better throughput
- **FxHashMap/FxHashSet**: `rustc_hash` — faster than std HashMap for string keys
- **Small strings**: Video IDs stored as 8-char strings (`uuid::Uuid::simple()`)

### yt-dlp / aria2c Pipeline
- **External downloader**: aria2c with 16 connections (`-x16 -s16`), resume, retry-wait, timeouts
- **Format selection**: Prioritizes itag 774 (Premium 256kbps Opus) → native Opus → any
- **Archive file**: `--download-archive` prevents re-downloads
- **JS runtime**: Bun for YouTube challenge solving (`--js-runtimes bun`)
- **Extractor args**: `youtube:player_client=web` — ensures cookie evaluation on web client

### SQLite
- **WAL mode**: Concurrent readers + single writer without locking
- **Busy timeout**: 5000ms handles contention
- **Single connection**: `Mutex<Connection>` — queue is small, mutations are fast
- **Persist debounced**: Every 2s + on changes (not every operation)

### Thumbnail & Tagging
- **ffmpeg compress**: Single pass, 320px max dimension, quality 5
- **lofty**: Native Rust Vorbis comments — no Python/mutagen boundary
- **Stream copy**: When yt-dlp outputs Opus, no re-encode (lossless)

---

## Configuration Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `BULKER_BASE` | `$HOME/Projects/Bulker` | Base directory for all relative paths |
| `COOKIES_FILE` | `cookies.txt` | Netscape-format YouTube Premium cookies |
| `TEMP_WORKSPACE_DIR` | `temp_workspace` | Temporary download workspace |
| `SONGS_ARCHIVE_DIR` | `Songs_archive` | Final organized archive |
| `QUEUE_DB_PATH` | `queue.db` | SQLite database |
| `LOG_FILE` | `app.log` | Log file |
| `HOST` | `127.0.0.1` | Server bind address |
| `PORT` | `8000` | Server port |
| `ARIA2C_CONNECTIONS` | `16` | aria2c split connections |
| `DEFAULT_CONCURRENCY` | `3` | Default concurrent downloads |
| `MAX_CONCURRENCY` | `6` | Max concurrent downloads |
| `SEARCH_RESULT_LIMIT` | `8` | Search results per query |
| `RATE_LIMIT_KIBS` | `0` | Global rate limit (0 = unlimited) |
| `THUMBNAIL_MAX_DIMENSION` | `320` | Thumbnail max px |
| `DOWNLOAD_RETRIES` | `10` | yt-dlp retries |
| `SOCKET_TIMEOUT` | `30` | Network timeout (seconds) |

---

## Usage Examples

### Start Server
```bash
cd rust
mise run server          # http://127.0.0.1:8000
# or
cargo run
```

### Queue Downloads
```bash
# From video IDs file (one per line)
cargo run -- queue-from-file ../video_ids.txt

# From phone (scan for ≤96kbps, queue re-downloads)
cargo run -- phone-scan --no-dry-run

# From URLs (via web UI or API)
curl -X POST http://127.0.0.1:8000/api/add \
  -H "Content-Type: application/json" \
  -d '{"raw_input": "https://music.youtube.com/watch?v=VIDEO_ID"}'
```

### Monitor & Manage
```bash
# Web UI
open http://127.0.0.1:8000

# CLI library view
cargo run -- songs

# Quality check (proves Premium 256kbps)
cargo run -- check "https://music.youtube.com/watch?v=VIDEO_ID"

# Archive TUI (bulk select/delete/move)
cargo run -- archive-tui
```

### Verify Premium Delivery
```bash
# Every download logs quality receipt in app.log:
# "Quality receipt for Title: itag Some("774"), delivered ~256 kbps"
# Delivered kbps = (file_size * 8 / 1000) / duration
# Premium target: ~256 kbps | Free tier: 128-160 kbps
```

---

## Development

```bash
# Install toolchain
mise install

# System deps
sudo pacman -S ffmpeg aria2  # or apt equivalent

# Check / lint / test
mise run check    # cargo check
mise run lint     # cargo clippy
mise run test     # cargo test
mise run quality  # fmt + check + lint + test

# Build release binary
mise run build    # cargo build --release
```

---

## License

MIT — see LICENSE file.