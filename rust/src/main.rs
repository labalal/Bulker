//! Bulker-rs: axum + SQLite rewrite of the Python/FastAPI Bulker app.
//!
//! Same HTTP API (so the unchanged static/index.html works), same yt-dlp +
//! aria2c + ffmpeg download engine (the reliability-critical part that is
//! deliberately NOT rewritten from scratch -- writing a YouTube protocol
//! client is weeks of brittle work with zero quality upside), same retry
//! pipeline. What changed:
//!   - FastAPI  -> axum single binary
//!   - queue_store.json -> SQLite (queue.db), including runtime settings
//!   - mutagen  -> lofty (native Rust Opus tagging)
//!   - the whole thing lives in one process with no Python runtime

mod adb_scanner;
mod config;
mod db;
mod log;
mod organizer;
pub mod page_cache;
mod perf;
pub mod plugin;
mod queue_organizer;
pub mod security;
mod sqlite;
mod tag;
mod tui;
mod ytdlp;

use adb_scanner::{AdbScanner, ScannedSong, build_search_query};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use mimalloc::MiMalloc;


use crate::config::config;
use crate::db::{Db, DbConfig, HistoryEntry, Item};
use crate::organizer::{MusicMetadata, extract_metadata_from_ytdlp};
use crate::queue_organizer::{QueueOrganizer, OrganizerConfig, QueueItem};
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
const HTML_SRC: &str = include_str!("../../web/index.html");

pub static STEPS: [&str; 5] = [
    "Download",
    "Extract Audio",
    "Thumbnail",
    "Metadata",
    "Archive",
];
type Steps = Vec<(String, String)>;

struct Settings {
    concurrency: usize,
    aria2_connections: i64,
    rate_limit_kibs: i64,
    thumbnail_max_dimension: i64,
}

struct AppState {
    db: Arc<Db>,
    plugin_manager: plugin::PluginManager,
    queue_organizer: QueueOrganizer,
    items: Mutex<FxHashMap<String, Item>>,
    order: Mutex<Vec<String>>,
    history: Mutex<Vec<HistoryEntry>>,
    steps: Mutex<FxHashMap<String, Steps>>,
    cancel_flags: Mutex<FxHashMap<String, Arc<AtomicBool>>>,
    settings: Mutex<Settings>,
    active_downloads: Mutex<usize>,
    active_large_downloads: Mutex<usize>,
}

static STATE: OnceLock<Arc<AppState>> = OnceLock::new();
fn state() -> &'static Arc<AppState> {
    STATE.get().expect("state initialized")
}

fn create_app_state(db_config: DbConfig, organizer_config: OrganizerConfig) -> Arc<AppState> {
    let db = Arc::new(Db::new(db_config).expect("Failed to create DB"));
    let plugin_manager = plugin::PluginManager::new(db.clone());
    plugin_manager.register(Arc::new(plugin::adb_phone::AdbPhonePlugin::new()));
    plugin_manager.register(Arc::new(plugin::template::CustomTemplatePlugin::new()));
    let queue_organizer = QueueOrganizer::new(organizer_config);

    Arc::new(AppState {
        db,
        plugin_manager,
        queue_organizer,
        items: Mutex::new(FxHashMap::default()),
        order: Mutex::new(Vec::new()),
        history: Mutex::new(Vec::new()),
        steps: Mutex::new(FxHashMap::default()),
        cancel_flags: Mutex::new(FxHashMap::default()),
        settings: Mutex::new(Settings {
            concurrency: config()
                .concurrency_default
                .clamp(1, config().concurrency_max),
            aria2_connections: config().aria2c_connections.clamp(1, 32),
            rate_limit_kibs: config().default_rate_limit_kibs.clamp(0, 1024 * 1024),
            thumbnail_max_dimension: config().thumbnail_max_dimension.clamp(80, 4096),
        }),
        active_downloads: Mutex::new(0),
        active_large_downloads: Mutex::new(0),
    })
}

fn fresh_steps() -> Steps {
    STEPS
        .iter()
        .map(|name| (name.to_string(), "pending".to_string()))
        .collect()
}

fn steps_map(steps: &Steps) -> BTreeMap<String, String> {
    steps.iter().cloned().collect()
}

/// Port of main.is_active_status.
fn is_active_status(status: &str, steps: &Steps) -> bool {
    if status == "Starting..." || status == "Cancelling..." {
        return true;
    }
    if status.starts_with("Retrying") {
        return true;
    }
    if steps.iter().any(|(_, s)| s == "active") {
        return true;
    }
    status.starts_with("Downloading") || status.starts_with("Processing")
}

fn iso_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let (y, m, d) = log::civil_from_days(days);
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        t / 3600,
        (t % 3600) / 60,
        t % 60
    )
}

fn new_item(url: String, title: String, from_playlist: bool) -> Item {
    Item {
        id: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
        url,
        title,
        from_playlist,
        status: "Waiting in queue...".to_string(),
    }
}

fn record_history(item: &Item) {
    let entry = HistoryEntry {
        id: String::new(), // SQLite assigns the real rowid
        title: item.title.clone(),
        status: item.status.clone(),
        from_playlist: item.from_playlist,
        finished_at: iso_now(),
    };
    if let Ok(mut history) = state().history.lock() {
        history.push(entry.clone());
    }
    let limit = config().history_limit;
    if let Err(e) = state().db.insert_history(&entry, limit) {
        error!("Failed to append history: {e}");
    }
}

fn persist_now() {
    let s = state();
    let (items, order) = match (s.items.lock(), s.order.lock()) {
        (Ok(items), Ok(order)) => (items, order),
        _ => return,
    };
    if let Err(e) = s.db.persist_items(&order, &items) {
        error!("Failed to save queue state: {e}");
    }
}

// --- worker machinery ------------------------------------------------------

async fn claim_next_item() -> Option<Item> {
    let s = state();
    let mut items = s.items.lock().unwrap();
    let order = s.order.lock().unwrap();
    for id in order.iter() {
        if let Some(item) = items.get_mut(id) {
            if item.status == "Waiting in queue..." {
                item.status = "Starting...".to_string();
                return Some(item.clone());
            }
        }
    }
    None
}

async fn gate_acquire() {
    loop {
        {
            // Guard scope kept strictly inside the block so the whole
            // future stays Send (no std MutexGuard may live across the
            // await below).
            let s = state();
            let limit = s.settings.lock().unwrap().concurrency;
            let mut active = s.active_downloads.lock().unwrap();
            if *active < limit {
                *active += 1;
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn gate_release() {
    let s = state();
    let mut active = s.active_downloads.lock().unwrap();
    *active = active.saturating_sub(1);
}

fn set_status(item: &Item, status: &str) {
    if let Ok(mut items) = state().items.lock() {
        if let Some(entry) = items.get_mut(&item.id) {
            entry.status = status.to_string();
        }
    }
}

fn set_step(item: &Item, step: &str, state_str: &str, detail: &str) {
    let s = state();
    if let Ok(mut items) = s.items.lock() {
        if let Some(entry) = items.get_mut(&item.id) {
            entry.status = detail.to_string();
        }
    }
    if let Ok(mut steps) = s.steps.lock() {
        if let Some(map) = steps.get_mut(&item.id) {
            for (name, st) in map.iter_mut() {
                if name == step {
                    *st = state_str.to_string();
                }
            }
        }
    }
}

/// One item's full download+tag lifecycle, including retry/backoff.
async fn process_item(item: &Item, cancel: Arc<AtomicBool>) {
    let cfg = config();
    let mut final_error: Option<String> = None;

    for attempt in 1..=cfg.auto_retry_attempts {
        if cancel.load(Ordering::Relaxed) {
            set_status(item, "Cancelled");
            info!("Cancelled: {}", item.title);
            final_error = None;
            break;
        }
        if attempt > 1 {
            if let Ok(mut steps) = state().steps.lock() {
                if let Some(map) = steps.get_mut(&item.id) {
                    *map = fresh_steps();
                }
            }
            info!(
                "Retrying ({attempt}/{}): {}",
                cfg.auto_retry_attempts, item.title
            );
        }
        info!("Starting download: {} ({})", item.title, item.url);

        // Progress plumbing: ytdlp::download_once sends "pct|42.3" updates
        // over the channel while aria2c runs; this task paints them onto
        // the item's status so the UI shows a live percentage.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let painter = tokio::spawn({
            let item = item.clone();
            async move {
                while let Some(msg) = rx.recv().await {
                    if let Some(("pct", pct)) = msg.split_once('|') {
                        set_step(
                            &item,
                            "Download",
                            "active",
                            &format!("Downloading... {pct}%"),
                        );
                    }
                }
            }
        });

        let (aria2, rate_limit) = {
            let settings = state().settings.lock().unwrap();
            (settings.aria2_connections, settings.rate_limit_kibs)
        };
        let params = ytdlp::DownloadParams {
            url: item.url.clone(),
            aria2_connections: aria2,
            rate_limit_kibs: rate_limit,
            cancel: cancel.clone(),
        };

        let result = ytdlp::download_once(&params, tx, painter).await;
        match result {
            Ok(meta) => {
                set_step(item, "Download", "done", "Download complete");
                set_step(
                    item,
                    "Extract Audio",
                    "active",
                    "Extracting audio (Opus)...",
                );
                let report = |step: &str, state_str: &str, detail: &str| {
                    set_step(item, step, state_str, detail);
                };
                // meta.opus_path / meta.thumb_path are moved into
                // finalize_pipeline; grab a clone of everything record_library
                // still needs before that move.
                let library = (
                    meta.title.clone(),
                    meta.artist.clone(),
                    meta.duration,
                    meta.best_opus_kbps,
                    meta.selected_format_id.clone(),
                    state().settings.lock().unwrap().thumbnail_max_dimension,
                );
                
                // Build metadata for organized archival
                let entry = crate::ytdlp::Entry {
                    title: meta.title.clone(),
                    url: item.url.clone(),
                    uploader: meta.artist.clone(),
                    duration: meta.duration,
                    from_playlist: item.from_playlist,
                };
                let archive_meta = extract_metadata_from_ytdlp(&entry, meta.duration);
                
                match tag::finalize_pipeline(
                    meta.opus_path,
                    meta.thumb_path,
                    &meta.title,
                    &meta.artist,
                    &cancel,
                    &report,
                    library.5,
                    &archive_meta,
                ) {
                    Ok(path) => {
                        info!("Completed: {} -> {}", item.title, path.display());
                        set_status(item, "Completed");
                        record_library(item, &library, &path);
                        record_history(item);
                        final_error = None;
                        break;
                    }
                    Err(e) if e == "__cancelled__" => {
                        set_status(item, "Cancelled");
                        record_history(item);
                        final_error = None;
                        break;
                    }
                    Err(e) => {
                        set_status(item, &format!("Error: {e}"));
                        info!("Failed: {} -- {e}", item.title);
                        final_error = None;
                        break;
                    }
                }
            }
            Err(e) if e == "__cancelled__" => {
                set_status(item, "Cancelled");
                info!("Cancelled: {}", item.title);
                cleanup_partial(item);
                record_history(item);
                final_error = None;
                break;
            }
            Err(e) => {
                warn!(
                    "Attempt {attempt}/{} failed for {} -- {e}",
                    cfg.auto_retry_attempts, item.title
                );
                final_error = Some(e.clone());
                if e.to_lowercase().contains("unavailable") {
                    // Retrying can never fix a gone/private/region-locked video.
                    break;
                }
                if attempt < cfg.auto_retry_attempts {
                    let delay = cfg.auto_retry_backoff_seconds * attempt as u64;
                    set_status(
                        item,
                        &format!(
                            "Retrying in {delay}s (attempt {}/{})",
                            attempt + 1,
                            cfg.auto_retry_attempts
                        ),
                    );
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
    }

    if let Some(err) = final_error {
        set_status(item, &format!("Error: {err}"));
        error!("Failed: {} -- {err}", item.title);
        record_history(item);
    }
}

fn cleanup_partial(item: &Item) {
    // Best-effort sweep of partial files for this video id.
    if let Some(id) = ytdlp::query_param(&item.url, "v") {
        if let Some((opus, thumb)) = ytdlp::locate_outputs(&id) {
            let _ = std::fs::remove_file(&opus);
            if let Some(t) = thumb {
                let _ = std::fs::remove_file(&t);
                let _ = std::fs::remove_file(t.with_extension("jpg"));
            }
            let _ = opus;
        }
    }
}

async fn worker() {
    loop {
        let queue_items = {
            let organizer = &state().queue_organizer;
            organizer.get_next_batch(1, None)
        };
        
        let Some(queue_item) = queue_items.into_iter().next() else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        
        let is_large = queue_item.estimated_size > 10 * 1024 * 1024;
        if is_large {
            loop {
                let count = *state().active_large_downloads.lock().unwrap();
                if count < state().queue_organizer.config.max_concurrent_large {
                    *state().active_large_downloads.lock().unwrap() += 1;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        
        let cancel = {
            let flags = state().cancel_flags.lock().unwrap();
            flags
                .get(&queue_item.id)
                .cloned()
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
        };
        
        let item = Item {
            id: queue_item.id.clone(),
            url: queue_item.url.clone(),
            title: queue_item.title.clone(),
            from_playlist: false,
            status: "Starting...".to_string(),
        };
        
        {
            let mut items = state().items.lock().unwrap();
            let mut order = state().order.lock().unwrap();
            let mut steps = state().steps.lock().unwrap();
            let mut flags = state().cancel_flags.lock().unwrap();
            
            items.insert(item.id.clone(), item.clone());
            order.push(item.id.clone());
            steps.insert(item.id.clone(), fresh_steps());
            flags.insert(item.id.clone(), Arc::new(AtomicBool::new(false)));
        }
        
        if cancel.load(Ordering::Relaxed) {
            set_status(&item, "Cancelled");
            record_history(&item);
            if is_large {
                *state().active_large_downloads.lock().unwrap() -= 1;
            }
            state().queue_organizer.mark_completed(&queue_item.id, 0).ok();
            continue;
        }
        
        gate_acquire().await;
        process_item(&item, cancel).await;
        gate_release();
        
        if is_large {
            *state().active_large_downloads.lock().unwrap() -= 1;
        }
        
        if item.status == "Completed" {
            state().queue_organizer.mark_completed(&queue_item.id, 0).ok();
            tokio::spawn(auto_remove_after_delay(
                item.id.clone(),
                config().auto_remove_delay_seconds,
            ));
        } else {
            state().queue_organizer.requeue_failed(&queue_item.id, &item.status).ok();
            persist_now();
        }
    }
}

async fn auto_remove_after_delay(item_id: String, delay: u64) {
    tokio::time::sleep(Duration::from_secs(delay)).await;
    let s = state();
    {
        let mut items = s.items.lock().unwrap();
        let mut order = s.order.lock().unwrap();
        if let Some(item) = items.get(&item_id) {
            if item.status == "Completed" {
                items.remove(&item_id);
                order.retain(|i| i != &item_id);
            }
        }
    }
    s.cancel_flags.lock().unwrap().remove(&item_id);
    s.steps.lock().unwrap().remove(&item_id);
    persist_now();
}

// --- startup ---------------------------------------------------------------

/// Port of downloader.cleanup_existing_mess: rescue stray audio out of the
/// legacy downloads folder into Songs_archive, delete obvious junk.
fn cleanup_existing_mess() {
    let cfg = config();
    for dir in [&cfg.songs_archive_dir, &cfg.temp_workspace_dir] {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(root) = std::fs::canonicalize(&cfg.temp_workspace_dir) else {
        return;
    };
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
                "opus" | "m4a" => {
                    if let Some(name) = path.file_name() {
                        if let Err(e) = std::fs::rename(
                            &path,
                            std::path::Path::new(&cfg.songs_archive_dir).join(name),
                        ) {
                            error!("Could not rescue {}: {e}", path.display());
                        }
                    }
                }
                "jpg" | "webp" | "part" | "webm" | "temp" => {
                    if let Err(e) = std::fs::remove_file(&path) {
                        error!("Could not remove leftover {}: {e}", path.display());
                    }
                }
                _ => {}
            }
        }
    }
}

/// Port of cleaning.cleanup_workspace.
fn cleanup_workspace() -> usize {
    fn identifier(name: &str) -> String {
        if let (Some(open), Some(close)) = (name.find('['), name.find(']')) {
            if close == open + 12 {
                return name[open + 1..close].to_string();
            }
        }
        name.split('.').next().unwrap_or(name).to_string()
    }
    fn identifiers(dir: &str) -> FxHashSet<String> {
        let mut ids = FxHashSet::default();
        let Ok(root) = std::fs::canonicalize(dir) else {
            return ids;
        };
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Some(n) = path.file_name().and_then(|n| n.to_str()) {
                    ids.insert(identifier(n));
                }
            }
        }
        ids
    }
    let cfg = config();
    let saved = identifiers(&cfg.songs_archive_dir)
        .into_iter()
        .collect::<FxHashSet<_>>();
    let mut removed = 0;
    if let Ok(rd) = std::fs::read_dir(&cfg.temp_workspace_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if saved.contains(&identifier(&name)) {
                let _ = std::fs::remove_file(&path);
                info!("cleanup_workspace: removed {name}");
                removed += 1;
            }
        }
    }
    info!(
        "cleanup_workspace: removed {removed} processed file(s) from {}",
        cfg.temp_workspace_dir
    );
    removed
}

fn load_state() {
    let s = state();
    match s.db.load_items() {
        Ok(items) => {
            let mut items_map = s.items.lock().unwrap();
            let mut order = s.order.lock().unwrap();
            for item in items {
                let status = if is_active_status(&item.status, &fresh_steps()) {
                    "Waiting in queue...".to_string()
                } else {
                    item.status.clone()
                };
                let id = item.id.clone();
                items_map.insert(id.clone(), Item { status, ..item });
                order.push(id);
            }
            info!("Loaded {} queued item(s) from SQLite", items_map.len());
        }
        Err(e) => error!("Failed to load queue, starting with an empty queue: {e}"),
    }
    match s.db.load_history(config().history_limit) {
        Ok(h) => {
            *s.history.lock().unwrap() = h;
        }
        Err(e) => error!("Failed to load history: {e}"),
    }
    // Restore runtime settings (concurrency, aria2, rate limit) so they
    // survive restarts -- something queue_store.json also did for nothing.
    let db_conc =
        s.db.get_setting("concurrency", config().concurrency_default as i64) as usize;
    let db_aria2 = s.db.get_setting("aria2_connections", config().aria2c_connections);
    let db_rate = s.db.get_setting("rate_limit_kibs", config().default_rate_limit_kibs);
    let mut settings = s.settings.lock().unwrap();
    settings.concurrency = db_conc.clamp(1, config().concurrency_max);
    settings.aria2_connections = db_aria2.clamp(1, 32);
    settings.rate_limit_kibs = db_rate.clamp(0, 1024 * 1024);
}

// --- HTTP handlers -----------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(HTML_SRC)
}

#[derive(Deserialize)]
struct BulkURLRequest {
    raw_input: String,
}

async fn add_to_queue(
    State(_): State<Arc<AppState>>,
    Json(req): Json<BulkURLRequest>,
) -> Json<serde_json::Value> {
    let urls = ytdlp::parse_input_urls(&req.raw_input);
    let mut added_ids = Vec::new();
    let mut duplicates = Vec::new();
    
    for raw_url in urls {
        // Enforce user data sanitization and prevent SSRF / CLI flag injection
        if let Err(e) = security::validate_youtube_url(&raw_url) {
            warn!("Skipping unsafe input URL '{}': {}", raw_url, e);
            continue;
        }

        let video_id = ytdlp::query_param(&raw_url, "v");
        let list_id = ytdlp::query_param(&raw_url, "list");

        if let Some(ref vid) = video_id {
            if let Err(e) = security::validate_youtube_id(vid) {
                warn!("Skipping invalid video id '{}': {}", vid, e);
                continue;
            }
        }
        if let Some(ref lid) = list_id {
            if let Err(e) = security::validate_youtube_id(lid) {
                warn!("Skipping invalid playlist id '{}': {}", lid, e);
                continue;
            }
        }

        let entries: Vec<ytdlp::Entry> = if video_id.is_some() && list_id.is_none() {
            let meta = ytdlp::fetch_metadata(&raw_url).await;
            vec![ytdlp::Entry {
                title: meta.title,
                url: raw_url.clone(),
                uploader: meta.artist,
                duration: meta.duration,
                from_playlist: false,
            }]
        } else if list_id.is_some() {
            ytdlp::list_entries(&raw_url).await
        } else {
            vec![ytdlp::Entry {
                title: raw_url.clone(),
                url: raw_url.clone(),
                uploader: String::new(),
                duration: None,
                from_playlist: false,
            }]
        };

        for entry in entries {
            let music_meta = extract_metadata_from_ytdlp(&entry, entry.duration);
            
            let meta = queue_organizer::ItemMetadata {
                duration: music_meta.duration,
                bitrate: None,
                sample_rate: None,
                channels: None,
                codec: Some("opus".to_string()),
                file_size: None,
                format_id: None,
                is_premium: true,
                quality_score: None,
            };
            
            let quality_report = state().queue_organizer.quality_analyzer.analyze(&meta);
            
            let queue_item = QueueItem {
                id: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
                url: entry.url.clone(),
                title: entry.title.clone(),
                artist: Some(entry.uploader.clone()),
                priority: 0,
                status: "pending".to_string(),
                estimated_size: estimate_file_size(&music_meta),
                actual_size: None,
                retry_count: 0,
                created_at: chrono::Utc::now().to_rfc3339(),
                started_at: None,
                completed_at: None,
                quality_score: Some(quality_report.score),
                metadata: meta,
            };
            
            match state().queue_organizer.add_item(queue_item) {
                Ok(queue_organizer::AddResult::Added { item }) => {
                    added_ids.push(item.id);
                }
                Ok(queue_organizer::AddResult::Duplicate { existing_id, fingerprint }) => {
                    duplicates.push(json!({"id": existing_id, "fingerprint": fingerprint}));
                }
                Err(e) => {
                    warn!("Failed to add item to queue: {}", e);
                }
            }
        }
    }
    info!("Added {} item(s) to queue, {} duplicates skipped", added_ids.len(), duplicates.len());
    persist_now();
    Json(json!({
        "message": format!("Added {} item(s)", added_ids.len()),
        "ids": added_ids,
        "duplicates": duplicates
    }))
}

fn estimate_file_size(meta: &MusicMetadata) -> i64 {
    let duration = meta.duration.unwrap_or(240.0);
    let bitrate = if meta.is_long_form { 128 } else { 256 };
    ((duration * bitrate as f64 * 1000.0 / 8.0) as i64).max(1_000_000)
}

#[derive(Deserialize)]
struct SearchQ {
    q: String,
    limit: Option<usize>,
}

async fn search(Query(q): Query<SearchQ>) -> Json<serde_json::Value> {
    if q.q.trim().is_empty() {
        return Json(json!({"results": []}));
    }
    let limit = q.limit.unwrap_or(config().search_result_limit);
    let results = ytdlp::search_videos(&q.q, limit).await;
    Json(json!({"results": results}))
}

async fn get_queue() -> Json<serde_json::Value> {
    let s = state();
    let (items, order, steps) = match (s.items.lock(), s.order.lock(), s.steps.lock()) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        _ => return Json(json!({"queue": []})),
    };
    let queue: Vec<serde_json::Value> = order
        .iter()
        .filter_map(|id| {
            let item = items.get(id)?;
            let steps = steps.get(id).cloned().unwrap_or_else(fresh_steps);
            Some(json!({
                "id": item.id, "url": item.url, "title": item.title,
                "from_playlist": item.from_playlist, "status": item.status,
                "steps": steps_map(&steps),
            }))
        })
        .collect();
    Json(json!({"queue": queue}))
}

/// Catalog the finished song into SQLite: what you have, at what quality.
fn record_library(
    item: &Item,
    library: &(String, String, Option<f64>, f64, Option<String>, i64),
    final_path: &std::path::Path,
) {
    let size_bytes = std::fs::metadata(final_path).map(|m| m.len()).unwrap_or(0);
    let delivered_kbps = library
        .2
        .filter(|d| *d > 0.0 && size_bytes > 0)
        .map(|d| (size_bytes * 8 / 1000) as f64 / d);
    let song = db::SongRow {
        video_id: ytdlp::query_param(&item.url, "v").unwrap_or_default(),
        title: library.0.clone(),
        artist: library.1.clone(),
        itag: library.4.clone(),
        abr: if library.3 > 0.0 {
            Some(library.3)
        } else {
            None
        },
        delivered_kbps,
        size_bytes: size_bytes as i64,
        path: final_path.display().to_string(),
        downloaded_at: iso_now(),
        sample_rate: None,
        channels: None,
        bit_depth: None,
    };
    let _ = state().db.insert_song(&song);
    let song_clone = song.clone();
    tokio::spawn(async move {
        state().plugin_manager.dispatch_song_downloaded(&song_clone).await;
    });
}

async fn get_history() -> Json<serde_json::Value> {
    let last50: Vec<HistoryEntry> = state()
        .history
        .lock()
        .unwrap()
        .iter()
        .rev()
        .take(50)
        .cloned()
        .collect();
    Json(json!({"history": last50}))
}

#[derive(Deserialize)]
struct ReorderRequest {
    ids: Vec<String>,
}

async fn reorder_queue(Json(req): Json<ReorderRequest>) -> Json<serde_json::Value> {
    let s = state();
    {
        let items = s.items.lock().unwrap();
        let mut order = s.order.lock().unwrap();
        let mut new_order: Vec<String> = req
            .ids
            .into_iter()
            .filter(|id| items.contains_key(id))
            .collect();
        for id in order.iter() {
            if !new_order.contains(id) {
                new_order.push(id.clone());
            }
        }
        *order = new_order;
    }
    persist_now();
    Json(json!({"message": "Reordered"}))
}

async fn delete_item(Path(item_id): Path<String>) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = state();
    let (status_snapshot, steps_snapshot) = {
        let items = s.items.lock().unwrap();
        let steps = s.steps.lock().unwrap();
        match items.get(&item_id) {
            Some(it) => Ok((
                it.status.clone(),
                steps.get(&item_id).cloned().unwrap_or_else(fresh_steps),
            )),
            None => Err(StatusCode::NOT_FOUND),
        }
    }?;
    if !is_active_status(&status_snapshot, &steps_snapshot) {
        {
            let mut items = s.items.lock().unwrap();
            let mut order = s.order.lock().unwrap();
            items.remove(&item_id);
            order.retain(|i| i != &item_id);
            s.steps.lock().unwrap().remove(&item_id);
            s.cancel_flags.lock().unwrap().remove(&item_id);
        }
        persist_now();
        return Ok(Json(json!({"message": "Removed"})));
    }
    if let Some(flag) = s.cancel_flags.lock().unwrap().get(&item_id) {
        flag.store(true, Ordering::Relaxed);
    }
    set_status_snapshot(&item_id, "Cancelling...");
    Ok(Json(json!({"message": "Cancel requested"})))
}

fn set_status_snapshot(item_id: &str, status: &str) {
    if let Ok(mut items) = state().items.lock() {
        if let Some(item) = items.get_mut(item_id) {
            item.status = status.to_string();
        }
    }
}

async fn retry_item(
    Path(item_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let s = state();
    {
        let mut items = s.items.lock().unwrap();
        let Some(item) = items.get_mut(&item_id) else {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"detail": "Item not found"})),
            ));
        };
        if !(item.status.starts_with("Error") || item.status == "Cancelled") {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": "Only failed or cancelled items can be retried"})),
            ));
        }
        item.status = "Waiting in queue...".to_string();
        let flag = Arc::new(AtomicBool::new(false));
        s.cancel_flags.lock().unwrap().insert(item_id.clone(), flag);
    }
    {
        let mut steps = s.steps.lock().unwrap();
        steps.insert(item_id.clone(), fresh_steps());
    }
    info!("Retrying: item {item_id}");
    persist_now();
    Ok(Json(json!({"message": "Retrying"})))
}

async fn clear_finished() -> Json<serde_json::Value> {
    let s = state();
    let removed = {
        let mut items = s.items.lock().unwrap();
        let mut order = s.order.lock().unwrap();
        let to_remove: Vec<String> = order
            .iter()
            .filter(|id| match items.get(*id) {
                Some(it) => {
                    it.status == "Completed"
                        || it.status == "Cancelled"
                        || it.status.starts_with("Error")
                }
                None => false,
            })
            .cloned()
            .collect();
        for id in &to_remove {
            items.remove(id);
        }
        order.retain(|i| !to_remove.contains(i));
        to_remove.len()
    };
    s.steps.lock().unwrap().clear();
    s.cancel_flags.lock().unwrap().clear();
    persist_now();
    Json(json!({"message": format!("Cleared {removed} item(s)")}))
}

async fn run_cleanup() -> Json<serde_json::Value> {
    let removed = tokio::task::spawn_blocking(cleanup_workspace)
        .await
        .unwrap_or(0);
    info!("Manual cleanup run via API: removed {removed} file(s)");
    Json(json!({"removed": removed}))
}

async fn get_settings() -> Json<serde_json::Value> {
    let s = state();
    let settings = s.settings.lock().unwrap();
    let cfg = config();
    Json(json!({
        "concurrency": settings.concurrency,
        "max_concurrency": cfg.concurrency_max,
        "aria2_connections": settings.aria2_connections,
        "rate_limit_kibs": settings.rate_limit_kibs,
        "thumbnail_max_dimension": settings.thumbnail_max_dimension,
        "cookies_expired": config::cookies_expired(),
        "temp_workspace_dir": cfg.temp_workspace_dir,
        "songs_archive_dir": cfg.songs_archive_dir,
        "cookies_file": cfg.cookies_file,
        "log_file": cfg.log_file,
    }))
}

#[derive(Deserialize)]
struct SettingsRequest {
    concurrency: Option<usize>,
    aria2_connections: Option<i64>,
    rate_limit_kibs: Option<i64>,
    thumbnail_max_dimension: Option<i64>,
}

async fn update_settings(Json(req): Json<SettingsRequest>) -> Json<serde_json::Value> {
    let s = state();
    let clamped = {
        let mut settings = s.settings.lock().unwrap();
        if let Some(c) = req.concurrency {
            settings.concurrency = c.clamp(1, config().concurrency_max);
            let _ = s.db.save_setting("concurrency", settings.concurrency as i64);
        }
        if let Some(a) = req.aria2_connections {
            settings.aria2_connections = a.clamp(1, 32);
            let _ = s.db.save_setting("aria2_connections", a);
        }
        if let Some(r) = req.rate_limit_kibs {
            settings.rate_limit_kibs = r.clamp(0, 1024 * 1024);
            let _ = s.db.save_setting("rate_limit_kibs", r);
        }
        if let Some(t) = req.thumbnail_max_dimension {
            settings.thumbnail_max_dimension = t.clamp(80, 4096);
            let _ = s.db.save_setting("thumbnail_max_dimension", t);
        }
        (
            settings.concurrency,
            settings.aria2_connections,
            settings.rate_limit_kibs,
            settings.thumbnail_max_dimension,
        )
    };
    Json(json!({
        "concurrency": clamped.0,
        "aria2_connections": clamped.1,
        "rate_limit_kibs": clamped.2,
        "thumbnail_max_dimension": clamped.3,
    }))
}

async fn acknowledge_cookies_warning() -> Json<serde_json::Value> {
    config::acknowledge_cookies_expiry();
    Json(json!({"message": "Cookie warning dismissed"}))
}

async fn get_logs(Query(q): Query<FxHashMap<String, String>>) -> Json<serde_json::Value> {
    let lines = q
        .get("lines")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200);
    Json(json!({"logs": log::read_last_lines(&config().log_file, lines)}))
}

#[derive(Deserialize)]
struct QualityQ {
    url: String,
}

async fn get_quality(Query(q): Query<QualityQ>) -> Json<serde_json::Value> {
    Json(ytdlp::check_quality(&q.url).await)
}

async fn get_songs() -> Json<serde_json::Value> {
    let (songs, total, total_bytes) = state().db.list_songs().unwrap_or((vec![], 0, 0));
    Json(json!({
        "songs": songs,
        "total": total,
        "total_bytes": total_bytes,
        "total_gib": (total_bytes as f64 / (1024.0 * 1024.0 * 1024.0) * 100.0).round() / 100.0,
    }))
}

#[derive(Deserialize)]
struct PluginCommandRequest {
    command: String,
    #[serde(default)]
    args: serde_json::Value,
}

async fn list_plugins() -> Json<serde_json::Value> {
    let plugins = state().plugin_manager.list_plugins();
    Json(json!({ "plugins": plugins }))
}

async fn execute_plugin_command(
    Path(plugin_id): Path<String>,
    Json(req): Json<PluginCommandRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match state().plugin_manager.execute_command(&plugin_id, &req.command, &req.args).await {
        Ok(result) => Ok(Json(result)),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

/// CLI subcommand: `bulker-rs check <url>` -- runs against yt-dlp directly,
/// no server needed. Part of the "CLI tools" role.
async fn cli_songs() {
    config::init();
    let Ok(_connect) = rusqlite::Connection::open(&config().queue_db_path) else {
        eprintln!(
            "error: no catalog yet ({} not found)",
            config().queue_db_path
        );
        std::process::exit(1);
    };
    let (songs, total, total_bytes) = {
        // Reuse the shared helpers through a temporary handle.
        let database = Db::new(DbConfig { path: config().queue_db_path.clone(), ..Default::default() }).unwrap();
        database.list_songs().unwrap_or((vec![], 0, 0))
    };
    if total == 0 {
        println!("library is empty");
        return;
    }
    let gib = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    println!("{total} song(s), {gib:.2} GiB spent of your disk budget:");
    for s in songs {
        let quality = s
            .delivered_kbps
            .map(|k| format!("{k:.0} kbps"))
            .unwrap_or_else(|| "n/a".into());
        println!(
            "  [{}] {} -- {} ({quality}, {itag})",
            s.video_id,
            s.title,
            s.artist,
            itag = s.itag.as_deref().unwrap_or("?")
        );
        println!("      {} @ {}", s.path, s.downloaded_at);
    }
}

async fn cli_check(url: &str) {
    config::init();
    let result = ytdlp::check_quality(url).await;
    println!(
        "{}",
        serde_json::to_string_pretty(&result).unwrap_or_default()
    );
    if result["premium_opus_available"].as_bool() == Some(true) {
        println!("\n[PREMIUM OK] 256kbps Opus (itag 774) available -- downloads will use it.");
        std::process::exit(0);
    }
    println!("\n[NOT PREMIUM] free-tier Opus only -- re-export Premium cookies.txt.");
    std::process::exit(1);
}

async fn cli_phone_scan(device_id: Option<String>, max_bitrate: Option<i32>, dry_run: bool) {
    config::init();

    let devices = AdbScanner::list_devices().unwrap_or_default();
    if devices.is_empty() {
        eprintln!("No ADB devices found. Enable USB debugging and connect your phone.");
        std::process::exit(1);
    }

    let scanner = AdbScanner::new(device_id.clone());
    
    println!("Scanning phone for music files...");
    let songs = scanner.scan_music_directories().unwrap_or_default();
    
    if songs.is_empty() {
        println!("No music files found on device.");
        return;
    }

    println!("Found {} total songs", songs.len());

    let max_br = max_bitrate.unwrap_or(96);
    let low_bitrate = scanner.filter_low_bitrate(&songs, max_br);
    
    println!("Found {} songs with bitrate <= {} kbps", low_bitrate.len(), max_br);

    for song in &low_bitrate {
        let br = song.bitrate.map_or("unknown".to_string(), |b| format!("{} kbps", b));
        println!("  [{}] {} - {} ({})", br, song.title, song.artist, song.file_path);
    }

    if dry_run {
        println!("\nDry run complete. Use --no-dry-run to queue downloads.");
        return;
    }

    log::init(&config().log_file);
    cleanup_existing_mess();
    let db_config = DbConfig {
        path: config().queue_db_path.clone(),
        pool_size: config().concurrency_max * 2,
        busy_timeout_ms: 10000,
        wal_mode: true,
        cache_size_kib: -65536,
        mmap_size_mib: 256,
        page_size: page_cache::SQLITE_PAGE_SIZE as u32,
        temp_store: crate::sqlite::TempStore::Memory,
    };
    let organizer_config = OrganizerConfig::default();
    let _ = STATE.set(create_app_state(db_config, organizer_config));

    println!("\nSearching and queuing {} songs for re-download...", low_bitrate.len());
    
    let mut queued = 0;
    for song in &low_bitrate {
        let query = build_search_query(song);
        match ytdlp::search_videos(&query, 1).await.first() {
            Some(entry) => {
                let s = state();
                let mut items = s.items.lock().unwrap();
                let mut order = s.order.lock().unwrap();
                let mut steps = s.steps.lock().unwrap();
                let mut flags = s.cancel_flags.lock().unwrap();
                
                let item = crate::new_item(entry.url.clone(), entry.title.clone(), false);
                items.insert(item.id.clone(), item.clone());
                order.push(item.id.clone());
                steps.insert(item.id.clone(), crate::fresh_steps());
                flags.insert(item.id.clone(), Arc::new(AtomicBool::new(false)));
                
                println!("  Queued: {} - {}", entry.title, entry.uploader);
                queued += 1;
            }
            None => {
                println!("  Not found: {}", query);
            }
        }
    }
    
    persist_now();
    println!("\nQueued {} songs for download.", queued);
}

async fn cli_phone_list(device_id: Option<String>) {
    config::init();

    let devices = AdbScanner::list_devices().unwrap_or_default();
    if devices.is_empty() {
        eprintln!("No ADB devices found. Enable USB debugging and connect your phone.");
        std::process::exit(1);
    }
    
    let scanner = AdbScanner::new(device_id);
    let songs = scanner.scan_music_directories().unwrap_or_default();
    
    if songs.is_empty() {
        println!("No music files found on device.");
        return;
    }

    println!("Found {} songs:", songs.len());
    for song in &songs {
        let br = song.bitrate.map_or("unknown".to_string(), |b| format!("{} kbps", b));
        println!("  [{}] {} - {} ({})", br, song.title, song.artist, song.file_path);
    }
}

async fn cli_queue_from_file(file_path: String) {
    config::init();
    log::init(&config().log_file);

    cleanup_existing_mess();
    let db_config = DbConfig {
        path: config().queue_db_path.clone(),
        pool_size: config().concurrency_max * 2,
        busy_timeout_ms: 10000,
        wal_mode: true,
        cache_size_kib: -65536,
        mmap_size_mib: 256,
        page_size: page_cache::SQLITE_PAGE_SIZE as u32,
        temp_store: crate::sqlite::TempStore::Memory,
    };
    let organizer_config = OrganizerConfig::default();
    let _ = STATE.set(create_app_state(db_config, organizer_config));

    let content = std::fs::read_to_string(&file_path).expect("Failed to read file");
    let video_ids: Vec<String> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect();

    println!("Queuing {} songs from {}...", video_ids.len(), file_path);
    let mut queued = 0;
    
    for id in video_ids {
        let url = format!("https://music.youtube.com/watch?v={}", id);
        println!("  Searching: {}", url);
        
        match ytdlp::search_videos(&url, 1).await.first() {
            Some(entry) => {
                let s = state();
                let mut items = s.items.lock().unwrap();
                let mut order = s.order.lock().unwrap();
                let mut steps = s.steps.lock().unwrap();
                let mut flags = s.cancel_flags.lock().unwrap();
                
                let item = crate::new_item(entry.url.clone(), entry.title.clone(), false);
                items.insert(item.id.clone(), item.clone());
                order.push(item.id.clone());
                steps.insert(item.id.clone(), crate::fresh_steps());
                flags.insert(item.id.clone(), Arc::new(AtomicBool::new(false)));
                
                println!("    ✓ Queued: {} - {}", entry.title, entry.uploader);
                queued += 1;
            }
            None => {
                println!("    ✗ Not found: {}", url);
            }
        }
    }
    
    persist_now();
    println!("\nDone! Queued {} songs.", queued);
    println!("Run 'cargo run' to start downloading.");
}

async fn cli_archive_tui() {
    config::init();
    let archive_dir = PathBuf::from(&config().songs_archive_dir);
    
    println!("Scanning archive directory: {}", archive_dir.display());
    let items = tui::scan_archive_dir(&archive_dir).unwrap_or_default();
    
    if items.is_empty() {
        println!("No songs found in archive.");
        return;
    }
    
    println!("Found {} songs. Starting TUI...", items.len());
    
    let mut tui = tui::ArchiveTui::new(items);
    match tui.run() {
        Ok(selected) => {
            println!("\nSelected {} items:", selected.len());
            for item in &selected {
                println!("  {} - {} ({})", item.artist, item.title, item.path.display());
            }
            // TODO: Add actions for selected items (delete, move, export)
        }
        Err(e) => {
            eprintln!("TUI error: {}", e);
        }
    }
}

// --- wiring ----------------------------------------------------------------

#[tokio::main]
async fn main() {
    // "CLI tools" role: check <url> / songs bypass the server entirely.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "check" {
        cli_check(&args[2]).await;
    } else if args.len() == 2 && args[1] == "songs" {
        cli_songs().await;
    } else if args.len() >= 2 && args[1] == "phone-scan" {
        let device_id = args.iter().position(|a| a == "-d" || a == "--device")
            .and_then(|i| args.get(i + 1))
            .cloned();
        let max_bitrate = args.iter().position(|a| a == "--max-bitrate")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse::<i32>().ok());
        let dry_run = !args.contains(&"--no-dry-run".to_string());
        cli_phone_scan(device_id, max_bitrate, dry_run).await;
    } else if args.len() >= 2 && args[1] == "phone-list" {
        let device_id = args.iter().position(|a| a == "-d" || a == "--device")
            .and_then(|i| args.get(i + 1))
            .cloned();
        cli_phone_list(device_id).await;
    } else if args.len() >= 2 && args[1] == "queue-from-file" {
        let file_path = args.get(2).expect("Usage: bulker-rs queue-from-file <file.txt>");
        cli_queue_from_file(file_path.clone()).await;
    } else if args.len() >= 2 && args[1] == "archive-tui" {
        config::init();
        cli_archive_tui().await;
    } else if args.len() > 1 {
        eprintln!("usage: bulker-rs [check <url> | songs | phone-scan [--device ID] [--max-bitrate N] [--no-dry-run] | phone-list [--device ID] | queue-from-file <file.txt> | archive-tui]  (no args = start the server/daemon)");
        std::process::exit(2);
    }

    config::init();
    log::init(&config().log_file);

    cleanup_existing_mess();
    
    let db_config = DbConfig {
        path: config().queue_db_path.clone(),
        pool_size: config().concurrency_max * 2,
        busy_timeout_ms: 10000,
        wal_mode: true,
        cache_size_kib: -65536,
        mmap_size_mib: 256,
        page_size: page_cache::SQLITE_PAGE_SIZE as u32,
        temp_store: crate::sqlite::TempStore::Memory,
    };
    
    let organizer_config = OrganizerConfig {
        max_queue_size: 10000,
        priority_boost_large: 1000,
        priority_penalty_retry: -500,
        dedup_threshold: 0.85,
        min_quality_score: 0.3,
        max_concurrent_large: config().concurrency_max / 2,
        large_threshold_bytes: 10 * 1024 * 1024,
    };
    
    let _ = STATE.set(create_app_state(db_config, organizer_config));

    let _ = std::fs::create_dir_all(&config().songs_archive_dir);
    let _ = std::fs::create_dir_all(&config().temp_workspace_dir);
    load_state();

    for _ in 0..config().concurrency_max {
        tokio::spawn(worker());
    }
    
    tokio::spawn(async {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            state().queue_organizer.rebalance_priorities();
        }
    });
    
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            persist_now();
        }
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/add", post(add_to_queue))
        .route("/api/search", get(search))
        .route("/api/queue", get(get_queue).delete(clear_finished))
        .route("/api/queue/reorder", patch(reorder_queue))
        .route("/api/queue/{item_id}", delete(delete_item))
        .route("/api/queue/{item_id}/retry", post(retry_item))
        .route("/api/cleanup", post(run_cleanup))
        .route("/api/settings", get(get_settings).post(update_settings))
        .route(
            "/api/cookies/acknowledge",
            post(acknowledge_cookies_warning),
        )
        .route("/api/history", get(get_history))
        .route("/api/logs", get(get_logs))
        .route("/api/quality", get(get_quality))
        .route("/api/songs", get(get_songs))
        .route("/api/plugins", get(list_plugins))
        .route("/api/plugins/{id}/command", post(execute_plugin_command))
        .with_state(state().clone());

    let addr = format!("{}:{}", config().host, config().port);
    info!("Bulker-rs listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}
