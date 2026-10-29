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

mod config;
mod db;
mod log;
mod tag;
mod ytdlp;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

use crate::config::config;
use crate::db::{Db, HistoryEntry, Item};

const INDEX_HTML: &str = include_str!("../../web/index.html");

pub static STEPS: [&str; 5] = ["Download", "Extract Audio", "Thumbnail", "Metadata", "Archive"];
type Steps = Vec<(String, String)>;

struct Settings {
    concurrency: usize,
    aria2_connections: i64,
    rate_limit_kibs: i64,
}

struct AppState {
    db: Db,
    items: Mutex<HashMap<String, Item>>,
    order: Mutex<Vec<String>>,
    history: Mutex<Vec<HistoryEntry>>,
    steps: Mutex<HashMap<String, Steps>>,
    cancel_flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
    settings: Mutex<Settings>,
    active_downloads: Mutex<usize>,
}

static STATE: OnceLock<Arc<AppState>> = OnceLock::new();
fn state() -> &'static Arc<AppState> {
    STATE.get().expect("state initialized")
}

fn fresh_steps() -> Steps {
    STEPS.iter().map(|name| (name.to_string(), "pending".to_string())).collect()
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
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let (y, m, d) = log::civil_from_days(days);
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
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
    if let Err(e) = db::insert_history(&state().db, &entry, limit) {
        error!("Failed to append history: {e}");
    }
}

fn persist_now() {
    let s = state();
    let (items, order) = match (s.items.lock(), s.order.lock()) {
        (Ok(items), Ok(order)) => (items, order),
        _ => return,
    };
    if let Err(e) = db::persist_items(&s.db, &order, &items) {
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
            info!("Retrying ({attempt}/{}): {}", cfg.auto_retry_attempts, item.title);
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
                        set_step(&item, "Download", "active", &format!("Downloading... {pct}%"));
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
                set_step(item, "Extract Audio", "active", "Extracting audio (Opus)...");
                let report = |step: &str, state_str: &str, detail: &str| {
                    set_step(item, step, state_str, detail);
                };
                match tag::finalize_pipeline(
                    meta.opus_path,
                    meta.thumb_path,
                    &meta.title,
                    &meta.artist,
                    &cancel,
                    &report,
                ) {
                    Ok(path) => {
                        info!("Completed: {} -> {}", item.title, path.display());
                        set_status(item, "Completed");
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
                warn!("Attempt {attempt}/{} failed for {} -- {e}", cfg.auto_retry_attempts, item.title);
                final_error = Some(e.clone());
                if e.to_lowercase().contains("unavailable") {
                    // Retrying can never fix a gone/private/region-locked video.
                    break;
                }
                if attempt < cfg.auto_retry_attempts {
                    let delay = cfg.auto_retry_backoff_seconds * attempt as u64;
                    set_status(
                        item,
                        &format!("Retrying in {delay}s (attempt {}/{})", attempt + 1, cfg.auto_retry_attempts),
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
        let Some(item) = claim_next_item().await else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let cancel = {
            let flags = state().cancel_flags.lock().unwrap();
            flags.get(&item.id).cloned().unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
        };

        if cancel.load(Ordering::Relaxed) {
            set_status(&item, "Cancelled");
            record_history(&item);
            continue;
        }

        gate_acquire().await;
        process_item(&item, cancel).await;
        gate_release();

        if item.status == "Completed" {
            tokio::spawn(auto_remove_after_delay(
                item.id.clone(),
                config().auto_remove_delay_seconds,
            ));
        } else {
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
    let Ok(root) = std::fs::canonicalize(&cfg.download_dir) else { return };
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
                "opus" | "m4a" => {
                    if let Some(name) = path.file_name() {
                        if let Err(e) = std::fs::rename(&path, std::env::current_dir().unwrap().join(&cfg.songs_archive_dir).join(name)) {
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
    fn identifiers(dir: &str) -> std::collections::HashSet<String> {
        let mut ids = std::collections::HashSet::new();
        let Ok(root) = std::fs::canonicalize(dir) else { return ids };
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
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
        .union(&identifiers(&cfg.download_dir))
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let mut removed = 0;
    if let Ok(rd) = std::fs::read_dir(&cfg.temp_workspace_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if saved.contains(&identifier(&name)) {
                let _ = std::fs::remove_file(&path);
                info!("cleanup_workspace: removed {name}");
                removed += 1;
            }
        }
    }
    info!("cleanup_workspace: removed {removed} processed file(s) from {}", cfg.temp_workspace_dir);
    removed
}

fn load_state() {
    let s = state();
    match db::load_items(&s.db) {
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
                items_map.insert(
                    id.clone(),
                    Item { status, ..item },
                );
                order.push(id);
            }
            info!("Loaded {} queued item(s) from SQLite", items_map.len());
        }
        Err(e) => error!("Failed to load queue, starting with an empty queue: {e}"),
    }
    match db::load_history(&s.db) {
        Ok(h) => {
            *s.history.lock().unwrap() = h;
        }
        Err(e) => error!("Failed to load history: {e}"),
    }
    // Restore runtime settings (concurrency, aria2, rate limit) so they
    // survive restarts -- something queue_store.json also did for nothing.
    let db_conc = db::get_setting(&s.db, "concurrency", config().concurrency_default as i64) as usize;
    let db_aria2 = db::get_setting(&s.db, "aria2_connections", config().aria2c_connections);
    let db_rate = db::get_setting(&s.db, "rate_limit_kibs", config().default_rate_limit_kibs);
    let mut settings = s.settings.lock().unwrap();
    settings.concurrency = db_conc.clamp(1, config().concurrency_max);
    settings.aria2_connections = db_aria2.clamp(1, 32);
    settings.rate_limit_kibs = db_rate.clamp(0, 1024 * 1024);
}

// --- HTTP handlers -----------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[derive(Deserialize)]
struct BulkURLRequest {
    raw_input: String,
}

async fn add_to_queue(State(_): State<Arc<AppState>>, Json(req): Json<BulkURLRequest>) -> Json<serde_json::Value> {
    let urls = ytdlp::parse_input_urls(&req.raw_input);
    let mut added_ids = Vec::new();
    for raw_url in urls {
        let entries = ytdlp::list_entries(&raw_url).await;
        let s = state();
        let mut items = s.items.lock().unwrap();
        let mut order = s.order.lock().unwrap();
        let mut steps = s.steps.lock().unwrap();
        let mut flags = s.cancel_flags.lock().unwrap();
        for entry in entries {
            let item = new_item(entry.url, entry.title, entry.from_playlist);
            items.insert(item.id.clone(), item.clone());
            order.push(item.id.clone());
            steps.insert(item.id.clone(), fresh_steps());
            flags.insert(item.id.clone(), Arc::new(AtomicBool::new(false)));
            added_ids.push(item.id);
        }
    }
    info!("Added {} item(s) to queue", added_ids.len());
    persist_now();
    Json(json!({"message": format!("Added {} item(s)", added_ids.len()), "ids": added_ids}))
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
        let mut new_order: Vec<String> = req.ids.into_iter().filter(|id| items.contains_key(id)).collect();
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

async fn retry_item(Path(item_id): Path<String>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let s = state();
    {
        let mut items = s.items.lock().unwrap();
        let Some(item) = items.get_mut(&item_id) else {
            return Err((StatusCode::NOT_FOUND, Json(json!({"detail": "Item not found"}))));
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
            .filter(|id| {
                match items.get(*id) {
                    Some(it) => it.status == "Completed" || it.status == "Cancelled" || it.status.starts_with("Error"),
                    None => false,
                }
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
    let removed = tokio::task::spawn_blocking(cleanup_workspace).await.unwrap_or(0);
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
}

async fn update_settings(Json(req): Json<SettingsRequest>) -> Json<serde_json::Value> {
    let s = state();
    let clamped = {
        let mut settings = s.settings.lock().unwrap();
        if let Some(c) = req.concurrency {
            settings.concurrency = c.clamp(1, config().concurrency_max);
            db::save_setting(&s.db, "concurrency", settings.concurrency as i64);
        }
        if let Some(a) = req.aria2_connections {
            settings.aria2_connections = a.clamp(1, 32);
            db::save_setting(&s.db, "aria2_connections", a);
        }
        if let Some(r) = req.rate_limit_kibs {
            settings.rate_limit_kibs = r.clamp(0, 1024 * 1024);
            db::save_setting(&s.db, "rate_limit_kibs", r);
        }
        (settings.concurrency, settings.aria2_connections, settings.rate_limit_kibs)
    };
    Json(json!({
        "concurrency": clamped.0,
        "aria2_connections": clamped.1,
        "rate_limit_kibs": clamped.2,
    }))
}

async fn acknowledge_cookies_warning() -> Json<serde_json::Value> {
    config::acknowledge_cookies_expiry();
    Json(json!({"message": "Cookie warning dismissed"}))
}

async fn get_logs(Query(q): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
    let lines = q.get("lines").and_then(|v| v.parse::<usize>().ok()).unwrap_or(200);
    Json(json!({"logs": log::read_last_lines(&config().log_file, lines)}))
}

#[derive(Deserialize)]
struct QualityQ {
    url: String,
}

async fn get_quality(Query(q): Query<QualityQ>) -> Json<serde_json::Value> {
    Json(ytdlp::check_quality(&q.url).await)
}

/// CLI subcommand: `bulker-rs check <url>` -- runs against yt-dlp directly,
/// no server needed. Part of the "CLI tools" role.
async fn cli_check(url: &str) {
    config::init();
    let result = ytdlp::check_quality(url).await;
    println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
    if result["premium_opus_available"].as_bool() == Some(true) {
        println!("\n[PREMIUM OK] 256kbps Opus (itag 774) available -- downloads will use it.");
        std::process::exit(0);
    }
    println!("\n[NOT PREMIUM] free-tier Opus only -- re-export Premium cookies.txt.");
    std::process::exit(1);
}

// --- wiring ----------------------------------------------------------------

#[tokio::main]
async fn main() {
    // "CLI tools" role: check <url> bypasses the server entirely.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "check" {
        cli_check(&args[2]).await;
    } else if args.len() > 1 && args[1] != "check" {
        eprintln!("usage: bulker-rs [check <url>]  (no args = start the server/daemon)");
        std::process::exit(2);
    }

    config::init();
    log::init(&config().log_file);

    cleanup_existing_mess();
    match db::init(&config().queue_db_path) {
        Ok(db) => {
            let _ = STATE.set(Arc::new(AppState {
                db,
                items: Mutex::new(HashMap::new()),
                order: Mutex::new(Vec::new()),
                history: Mutex::new(Vec::new()),
                steps: Mutex::new(HashMap::new()),
                cancel_flags: Mutex::new(HashMap::new()),
                settings: Mutex::new(Settings {
                    concurrency: config().concurrency_default.clamp(1, config().concurrency_max),
                    aria2_connections: config().aria2c_connections.clamp(1, 32),
                    rate_limit_kibs: config().default_rate_limit_kibs.clamp(0, 1024 * 1024),
                }),
                active_downloads: Mutex::new(0),
            }));
        }
        Err(e) => {
            eprintln!("FATAL: could not init queue.db: {e:#}");
            std::process::exit(1);
        }
    }

    let _ = std::fs::create_dir_all(&config().songs_archive_dir);
    let _ = std::fs::create_dir_all(&config().temp_workspace_dir);
    load_state();

    for _ in 0..config().concurrency_max {
        tokio::spawn(worker());
    }
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
        .route("/api/cookies/acknowledge", post(acknowledge_cookies_warning))
        .route("/api/history", get(get_history))
        .route("/api/logs", get(get_logs))
        .route("/api/quality", get(get_quality))
        .with_state(state().clone());

    let addr = format!("{}:{}", config().host, config().port);
    info!("Bulker-rs listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}
