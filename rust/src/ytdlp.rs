use crate::config;
use crate::{info, warn};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Entry {
    pub title: String,
    pub url: String,
    pub uploader: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    pub from_playlist: bool,
}

// --- URL massaging (port of downloader.to_music_youtube_url etc.) -----------

fn parse_query(qs: &str) -> Vec<(String, String)> {
    qs.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (p.to_string(), String::new()),
        })
        .collect()
}

pub fn query_param(url: &str, name: &str) -> Option<String> {
    let qs = url.split_once('?')?.1.split('#').next()?;
    parse_query(qs).into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Rewrites youtube.com / youtu.be / m.youtube.com links into the
/// equivalent music.youtube.com link. Quality control, not cosmetics:
/// YouTube only exposes the ~256kbps Music-only Opus itag (774) through the
/// Music surface, so every input URL must go in as music.youtube.com.
pub fn to_music_youtube_url(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .trim_end_matches('/');
    let (host_path, raw_query) = rest.split_once('?').unwrap_or((rest, ""));
    let host_path = host_path.split('#').next().unwrap_or(host_path);
    let (host, path) = match host_path.split_once('/') {
        Some((h, p)) => (h.to_lowercase(), p.to_string()),
        None => (host_path.to_lowercase(), String::new()),
    };
    let query = raw_query.split('#').next().unwrap_or(raw_query);

    if host == "music.youtube.com" {
        return url.to_string();
    }
    if host != "www.youtube.com" && host != "youtube.com" && host != "m.youtube.com" && host != "youtu.be" {
        return url.to_string(); // not a YouTube URL at all -- leave alone
    }

    let qs = parse_query(query);
    let video_id = if host == "youtu.be" {
        path.split('/').next().filter(|s| !s.is_empty()).map(str::to_string)
    } else if path.starts_with("shorts/") {
        path.split("shorts/").nth(1).and_then(|s| s.split('/').next()).map(str::to_string)
    } else {
        qs.iter().find(|(k, _)| k == "v").map(|(_, v)| v.clone())
    };

    if let Some(v) = video_id {
        let mut params = vec![format!("v={v}")];
        if let Some((_, list)) = qs.iter().find(|(k, _)| k == "list") {
            params.push(format!("list={list}"));
        }
        return format!("https://music.youtube.com/watch?{}", params.join("&"));
    }
    if let Some((_, list)) = qs.iter().find(|(k, _)| k == "list") {
        return format!("https://music.youtube.com/playlist?list={list}");
    }
    url.to_string() // no video/playlist id -- leave alone
}

/// Strips share/tracking params (si, ab_channel, pp, feature) the way
/// downloader.parse_input_urls did, then routes each URL through the
/// Music rewrite. Music-only policy: non-YouTube inputs are dropped with a
/// warning, not queued as mystery URLs.
pub fn parse_input_urls(raw: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for line in raw.lines() {
        let cleaned = line.trim();
        if cleaned.is_empty() {
            continue;
        }
        let stripped = match cleaned.split_once('?') {
            Some((base, qs)) => {
                let kept: Vec<String> = parse_query(qs)
                    .into_iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "si" | "ab_channel" | "pp" | "feature"))
                    .map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") })
                    .collect();
                if kept.is_empty() {
                    base.to_string()
                } else {
                    format!("{base}?{}", kept.join("&"))
                }
            }
            None => cleaned.to_string(),
        };
        let music_url = to_music_youtube_url(&stripped);
        if !music_url.starts_with("https://music.youtube.com") {
            warn!("Skipping non-YouTube-Music input: {cleaned}");
            continue;
        }
        urls.push(music_url);
    }
    urls
}

// --- yt-dlp JSON helpers ----------------------------------------------------

fn base_args() -> Vec<String> {
    let mut args = vec!["--quiet".to_string()];
    if std::path::Path::new(&config::config().cookies_file).exists() {
        args.push("--cookies".into());
        args.push(config::config().cookies_file.clone());
    }
    args.push("--extractor-args".into());
    args.push("youtube:player_client=android,web".into());
    args
}

async fn run_json(args: Vec<String>) -> Result<Option<serde_json::Value>, String> {
    let out = Command::new("yt-dlp").args(&args).output().await.map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if stdout.contains("cookies are no longer valid") || stderr.contains("cookies are no longer valid") {
        config::set_cookies_expired();
    }
    if !out.status.success() {
        let tail = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed");
        return Err(tail.trim().to_string());
    }
    Ok(serde_json::from_str::<serde_json::Value>(&stdout).ok())
}

fn flatten_entry(entry: &serde_json::Value) -> Option<(String, String)> {
    let raw = entry
        .get("url")
        .or_else(|| entry.get("webpage_url"))
        .or_else(|| entry.get("id"))?
        .as_str()?
        .to_string();
    let url = if raw.starts_with("http") {
        to_music_youtube_url(&raw)
    } else {
        to_music_youtube_url(&format!("https://music.youtube.com/watch?v={raw}"))
    };
    let title = entry
        .get("title")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| url.clone());
    Some((title, url))
}

/// Expands a pasted URL into individual entries (playlists -> one entry per
/// video). On any failure falls back to a single unknown entry, like the
/// Python version does.
pub async fn list_entries(url: &str) -> Vec<Entry> {
    let music_url = to_music_youtube_url(url);
    let mut args = base_args();
    args.extend(["--dump-single-json".into(), "--flat-playlist".into(), music_url]);
    match run_json(args).await {
        Ok(Some(info)) => {
            if let Some(entries) = info.get("entries").and_then(|e| e.as_array()) {
                let out: Vec<Entry> = entries
                    .iter()
                    .filter_map(|e| {
                        let (title, url) = flatten_entry(e)?;
                        Some(Entry {
                            title,
                            url,
                            uploader: String::new(),
                            duration: e.get("duration").and_then(|d| d.as_f64()),
                            from_playlist: true,
                        })
                    })
                    .collect();
                if !out.is_empty() {
                    return out;
                }
            }
            let (title, url) =
                flatten_entry(&info).unwrap_or_else(|| (url.to_string(), url.to_string()));
            vec![Entry {
                title,
                url,
                uploader: String::new(),
                duration: info.get("duration").and_then(|d| d.as_f64()),
                from_playlist: false,
            }]
        }
        _ => vec![Entry {
            title: url.to_string(),
            url: url.to_string(),
            uploader: String::new(),
            duration: None,
            from_playlist: false,
        }],
    }
}

/// Search directly on the music.youtube.com surface (not a generic
/// ytsearch:) so results are Music-catalog songs with the high-bitrate
/// Music-only itags available.
pub async fn search_videos(query: &str, limit: usize) -> Vec<Entry> {
    if query.trim().is_empty() {
        return vec![];
    }
    let url = format!("https://music.youtube.com/search?q={}", percent_encode(query.trim()));
    let mut args = base_args();
    args.extend([
        "--dump-single-json".into(),
        "--flat-playlist".into(),
        "--playlist-items".into(),
        format!("1-{}", limit.max(1)),
        url,
    ]);
    let Ok(Some(info)) = run_json(args).await else {
        warn!("search_videos failed for query {query:?}");
        return vec![];
    };
    let Some(entries) = info.get("entries").and_then(|e| e.as_array()) else {
        return vec![];
    };
    entries
        .iter()
        .filter_map(|e| {
            let (title, url) = flatten_entry(e)?;
            Some(Entry {
                title,
                url,
                uploader: e
                    .get("uploader")
                    .or_else(|| e.get("channel"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string(),
                duration: e.get("duration").and_then(|d| d.as_f64()),
                from_playlist: false,
            })
        })
        .collect()
}

// --- The download itself ------------------------------------------------------

pub struct DownloadParams {
    pub url: String,
    pub aria2_connections: i64,
    pub rate_limit_kibs: i64,
    pub cancel: Arc<AtomicBool>,
}

/// Everything the tagging pipeline needs once yt-dlp finished successfully.
pub struct DownloadMeta {
    pub opus_path: PathBuf,
    pub thumb_path: Option<PathBuf>,
    pub title: String,
    pub artist: String,
    pub duration: Option<f64>,
    pub premium_opus_available: bool,
    pub best_opus_kbps: f64,
    pub selected_format_id: Option<String>,
}

fn aria2_args(params: &DownloadParams) -> String {
    let n = params.aria2_connections;
    let mut a = format!("-x {n} -s {n} -k 1M");
    // Resilience against YouTube throttling (the "aria2c exited with code 2"
    // failure mode after ~2.5 stuck minutes): re-establish split
    // connections with a wait between attempts, bind real timeouts, resume
    // from the .aria2 control file, skip pre-allocation. The optional speed
    // cap goes to aria2c too -- yt-dlp's native ratelimit is ignored when
    // an external downloader is in use.
    a.push_str(" --continue=true --max-tries=20 --retry-wait=5 --connect-timeout=10 --timeout=60 --file-allocation=none");
    if params.rate_limit_kibs > 0 {
        a.push_str(&format!(" --max-overall-download-limit={}K", params.rate_limit_kibs));
    }
    a
}

/// Strips ANSI SGR escape sequences from a yt-dlp console line.
fn strip_ansi(line: &str) -> String {
    if !line.contains('\x1b') {
        return line.to_string();
    }
    let mut cleaned = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
        } else {
            cleaned.push(c);
        }
    }
    cleaned
}

/// Extracts the leading percentage from a `--newline` progress line like
/// "[download]  42.3% of 3.52MiB at ..." -- hand-parsed, no regex crate.
fn extract_percent(line: &str) -> Option<String> {
    let idx = line.find('%')?;
    let bytes = line.as_bytes();
    let mut start = idx;
    while start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.') {
        start -= 1;
    }
    let value: f64 = line[start..idx].parse().ok()?;
    Some(format!("{value:.1}"))
}

/// What the pre-pass -J lookup yields, including the Opus format inventory
/// so we can prove whether Premium's 256kbps itag (774) is on the table.
pub struct MetaPass {
    pub title: String,
    pub artist: String,
    pub duration: Option<f64>,
    pub premium_opus_available: bool,
    pub best_opus_kbps: f64,
    pub selected_format_id: Option<String>,
}

/// Metadata + format-list pre-pass: one cheap JSON lookup for tagging and
/// the quality receipt (artist -> uploader -> Unknown, same fallback chain
/// as the Python OpusTagPP).
pub async fn fetch_metadata(url: &str) -> MetaPass {
    let mut args = base_args();
    args.extend(["--dump-single-json".into(), "--no-playlist".into(), url.to_string()]);
    let Ok(Some(info)) = run_json(args).await else {
        return MetaPass {
            title: "Unknown Title".into(),
            artist: "Unknown Artist".into(),
            duration: None,
            premium_opus_available: false,
            best_opus_kbps: 0.0,
            selected_format_id: None,
        };
    };
    let title = info
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("Unknown Title")
        .to_string();
    let artist = info
        .get("artist")
        .or_else(|| info.get("uploader"))
        .and_then(|a| a.as_str())
        .filter(|a| !a.is_empty())
        .unwrap_or("Unknown Artist")
        .to_string();

    // Inventory every Opus stream this cookie session was offered.
    let mut opus_rates: Vec<(String, f64)> = Vec::new();
    for f in info.get("formats").and_then(|f| f.as_array()).into_iter().flatten() {
        let codec = f.get("acodec").and_then(|c| c.as_str()).unwrap_or("");
        let ext = f.get("ext").and_then(|e| e.as_str()).unwrap_or("");
        if !(codec.starts_with("opus") || ext == "opus") {
            continue;
        }
        let itag = f.get("format_id").and_then(|i| i.as_str()).unwrap_or("?").to_string();
        let abr = f.get("abr").and_then(|a| a.as_f64());
        let tbr = f.get("tbr").and_then(|a| a.as_f64());
        opus_rates.push((itag, abr.or(tbr).unwrap_or(0.0)));
    }
    let best_opus_kbps = opus_rates.iter().map(|(_, r)| *r).fold(0.0, f64::max);
    let premium_opus_available = best_opus_kbps >= 250.0;
    MetaPass {
        title,
        artist,
        duration: info.get("duration").and_then(|d| d.as_f64()),
        premium_opus_available,
        best_opus_kbps,
        selected_format_id: info.get("format_id").and_then(|f| f.as_str()).map(str::to_string),
    }
}

/// The `check` feature (CLI + /api/quality): prove whether the current
/// cookies get Premium's ~256kbps Opus (itag 774) for this video.
pub async fn check_quality(url: &str) -> serde_json::Value {
    let meta = fetch_metadata(&to_music_youtube_url(url)).await;
    let verdict = if meta.premium_opus_available {
        "Premium unlocked: 256kbps Opus available, downloads will use it"
    } else {
        "NOT premium-capable: only free-tier Opus offered (or no Opus at all)"
    };
    serde_json::json!({
        "title": meta.title,
        "premium_opus_available": meta.premium_opus_available,
        "best_opus_kbps": meta.best_opus_kbps,
        "selected_format_id": meta.selected_format_id,
        "verdict": verdict,
    })
}

fn look_for_cookie_warning(text: &str) {
    if text.contains("cookies are no longer valid") {
        config::set_cookies_expired();
    }
}

/// Runs one full yt-dlp (+ aria2c) download attempt into temp_workspace,
/// streaming progress as "pct|<number>" messages over `msg_tx` while it
/// runs. Returns the outputs + metadata on success; on cancel it returns
/// Err("__cancelled__"); other failures return the tail of yt-dlp's stderr.
pub async fn download_once(
    params: &DownloadParams,
    msg_tx: mpsc::UnboundedSender<String>,
    painter: tokio::task::JoinHandle<()>,
) -> Result<DownloadMeta, String> {
    // 1. Cheap metadata lookup for tagging + the quality receipt.
    let meta_pass = fetch_metadata(&params.url).await;
    if meta_pass.premium_opus_available {
        info!("Premium 256kbps Opus available for: {}", meta_pass.title);
    } else {
        warn!("No 256kbps Premium Opus offered for: {} (free-tier only)", meta_pass.title);
    }
    let video_id = query_param(&params.url, "v").unwrap_or_default();

    if params.cancel.load(Ordering::Relaxed) {
        return Err("__cancelled__".to_string());
    }

    // 2. The actual download.
    let cfg = config::config();
    let args = vec![
        "--newline".to_string(),
        "--no-playlist".to_string(),
        "--no-overwrites".to_string(),
        "--continue".to_string(),
        "--write-thumbnail".to_string(),
        "--convert-thumbnails".to_string(),
        "jpg".to_string(),
        "--extract-audio".to_string(),
        "--audio-format".to_string(),
        "opus".to_string(),
        // Only used if yt-dlp has to transcode a non-Opus source; when
        // source == Opus, yt-dlp stream-copies losslessly and ignores it.
        "--audio-quality".to_string(),
        "256".to_string(),
        "--retries".to_string(),
        cfg.download_retries.to_string(),
        "--fragment-retries".to_string(),
        cfg.download_retries.to_string(),
        "--socket-timeout".to_string(),
        cfg.socket_timeout.to_string(),
        "--download-archive".to_string(),
        cfg.archive_file.clone(),
        "--cookies".to_string(),
        cfg.cookies_file.clone(),
        "--external-downloader".to_string(),
        "aria2c".to_string(),
        "--external-downloader-args".to_string(),
        format!("aria2c:{}", aria2_args(params)),
        "--outtmpl".to_string(),
        format!("{}/%(title)s [%(id)s].%(ext)s", cfg.temp_workspace_dir),
        "--extractor-args".to_string(),
        "youtube:player_client=android,web".to_string(),
        params.url.clone(),
    ];

    let mut child = Command::new("yt-dlp")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn yt-dlp: {e}"))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let reader_out = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let clean = strip_ansi(&line);
            look_for_cookie_warning(&clean);
            if clean.starts_with("[download]") {
                if let Some(pct) = extract_percent(&clean) {
                    let _ = msg_tx.send(format!("pct|{pct}"));
                }
            }
        }
    });
    let reader_err = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut collected: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            look_for_cookie_warning(&line);
            collected.push(line);
        }
        collected
    });

    // Watch for cancellation while the download runs.
    let cancel = params.cancel.clone();
    let watcher = tokio::spawn(async move {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });

    let exit_status = tokio::select! {
        status = child.wait() => status.map_err(|e| e.to_string())?,
        _ = watcher => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            painter.abort();
            return Err("__cancelled__".to_string());
        }
    };
    // The painter task exits on its own: every msg_tx clone is dropped by
    // the time we get here, so its recv loop ends naturally.
    let _ = reader_out.await;
    let errors = reader_err.await.unwrap_or_default();

    if !exit_status.success() {
        let tail = errors
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty() && !l.contains("[download]"))
            .cloned()
            .unwrap_or_else(|| format!("yt-dlp exited with {exit_status}"));
        return Err(tail.trim().to_string());
    }

    // 3. Locate the outputs yt-dlp just wrote ("<title> [<id>].opus").
    if video_id.is_empty() {
        return Err("Could not determine video id from URL".to_string());
    }
    let (opus_path, thumb_path) = crate::ytdlp::locate_outputs(&video_id)
        .ok_or_else(|| format!("No .opus output found in {} for id {video_id}", cfg.temp_workspace_dir))?;

    // Quality receipt: delivered bitrate from size + duration, so Premium
    // (itag 774, ~256kbps) vs the free tier (itag 251, 128-160kbps) is
    // provable per file.
    let delivered_kbps = meta_pass.duration.map(|duration| {
        let size = std::fs::metadata(&opus_path).map(|m| m.len()).unwrap_or(0);
        (size * 8 / 1000) as f64 / duration.max(1.0)
    });
    if let Some(kbps) = delivered_kbps {
        info!(
            "Quality receipt for {}: itag {:?}, delivered ~{kbps:.0} kbps \
             (Premium target ~256kbps; free tier 128-160kbps)",
            meta_pass.title,
            meta_pass.selected_format_id,
        );
        if kbps < 200.0 {
            warn!("Delivered bitrate below 200kbps -- cookies may not be Premium for this track.");
        }
    }

    Ok(DownloadMeta {
        opus_path,
        thumb_path,
        title: meta_pass.title,
        artist: meta_pass.artist,
        duration: meta_pass.duration,
        premium_opus_available: meta_pass.premium_opus_available,
        best_opus_kbps: meta_pass.best_opus_kbps,
        selected_format_id: meta_pass.selected_format_id,
    })
}

/// Finds the .opus (+ thumbnail) files yt-dlp wrote for this video id in
/// temp_workspace. yt-dlp names them "<title> [<id>].<ext>".
pub fn locate_outputs(video_id: &str) -> Option<(PathBuf, Option<PathBuf>)> {
    let dir = std::path::Path::new(&config::config().temp_workspace_dir);
    let needle = format!("[{video_id}].");
    let mut opus = None;
    let mut thumb = None;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(&format!("{needle}opus")) {
                opus = Some(entry.path());
            } else if name.ends_with(&format!("{needle}jpg")) {
                thumb = Some(entry.path());
            }
        }
    }
    opus.map(|o| (o, thumb))
}
