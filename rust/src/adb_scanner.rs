use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use crate::security::{validate_device_id, sanitize_filename};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScannedSong {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub bitrate: Option<i32>,
    pub file_path: String,
    pub file_size: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StorageInfo {
    pub total: u64,
    pub free: u64,
    pub available: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AdbDirEntry {
    pub name: String,
    pub path: String,
    pub mtime: Option<String>,
}

pub struct AdbScanner {
    device_id: Option<String>,
}

impl AdbScanner {
    pub fn new(device_id: Option<String>) -> Self {
        Self { device_id }
    }

    fn adb_cmd(&self, args: &[&str]) -> Result<Command> {
        let mut cmd = Command::new("adb");
        if let Some(ref id) = self.device_id {
            let clean_id = validate_device_id(id)?;
            cmd.args(["-s", &clean_id]);
        }
        cmd.args(args);
        Ok(cmd)
    }

    pub fn list_devices() -> Result<Vec<String>> {
        let output = Command::new("adb")
            .args(["devices"])
            .output()
            .context("Failed to run adb devices")?;
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let devices: Vec<String> = stdout
            .lines()
            .skip(1)
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1] == "device" {
                    validate_device_id(parts[0]).ok()
                } else {
                    None
                }
            })
            .collect();
        
        Ok(devices)
    }

    pub fn scan_music_directories(&self) -> Result<Vec<ScannedSong>> {
        let music_dirs = vec![
            "/sdcard/Music",
            "/storage/emulated/0/Music",
            "/mnt/sdcard/Music",
            "/sdcard/emulated/music",
            "/storage/emulated/0/Download",
            "/mnt/sdcard/Download",
            "/storage/sdcard1",
            "/storage/sdcard2",
            "/mnt/media_rw",
        ];

        let mut all_songs = Vec::new();

        for dir in music_dirs {
            match self.scan_directory(dir) {
                Ok(songs) => {
                    println!("Found {} songs in {}", songs.len(), dir);
                    all_songs.extend(songs);
                }
                Err(e) => {
                    eprintln!("Warning: Could not scan {}: {}", dir, e);
                }
            }
        }

        Ok(all_songs)
    }

    pub fn scan_directory(&self, dir: &str) -> Result<Vec<ScannedSong>> {
        let extensions = ["opus", "mp3", "flac", "m4a", "ogg", "wav"];
        let mut all_files = Vec::new();
        
        let mut cmd = self.adb_cmd(&["shell", "ls", dir])?;
        if let Ok(output) = cmd.output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.contains("No such file") {
                        continue;
                    }
                    for ext in &extensions {
                        if trimmed.ends_with(&format!(".{ext}")) {
                            all_files.push(format!("{dir}/{trimmed}"));
                            break;
                        }
                    }
                }
            }
        }

        let mut songs = Vec::new();
        for file in all_files {
            if let Ok(song) = self.extract_metadata(&file) {
                songs.push(song);
            }
        }

        Ok(songs)
    }

    fn extract_metadata(&self, file_path: &str) -> Result<ScannedSong> {
        let mut cmd = self.adb_cmd(&[
            "shell",
            "mediainfo",
            "--Inform=General;%Title%|%Performer%|%Album%|%Duration%|%BitRate%",
            file_path
        ])?;
        let output = cmd.output().context("Failed to run mediainfo")?;

        let mut title = String::new();
        let mut artist = String::new();
        let mut album = None;
        let mut duration = None;
        let mut bitrate = None;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let parts: Vec<&str> = stdout.split('|').collect();
            
            if parts.len() >= 5 {
                if !parts[0].is_empty() { title = parts[0].to_string(); }
                if !parts[1].is_empty() { artist = parts[1].to_string(); }
                if !parts[2].is_empty() { album = Some(parts[2].to_string()); }
                if let Ok(d) = parts[3].parse::<f64>() { duration = Some(d / 1000.0); }
                if let Ok(b) = parts[4].parse::<i32>() { bitrate = Some(b / 1000); }
            }
        }

        if title.is_empty() {
            title = PathBuf::from(file_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown")
                .to_string();
        }

        if artist.is_empty() {
            artist = "Unknown Artist".to_string();
        }

        let title = sanitize_filename(&title);
        let artist = sanitize_filename(&artist);
        let file_size = self.get_file_size(file_path).unwrap_or(0);

        Ok(ScannedSong {
            title,
            artist,
            album,
            duration,
            bitrate,
            file_path: file_path.to_string(),
            file_size,
        })
    }

    fn get_file_size(&self, file_path: &str) -> Result<i64> {
        let mut cmd = self.adb_cmd(&["shell", "stat", "-c", "%s", file_path])?;
        let output = cmd.output().context("Failed to get file size")?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Ok(size) = stdout.parse::<i64>() {
                return Ok(size);
            }
        }
        Ok(0)
    }

    pub fn filter_low_bitrate(&self, songs: &[ScannedSong], max_bitrate: i32) -> Vec<ScannedSong> {
        songs
            .iter()
            .filter(|s| s.bitrate.map_or(true, |b| b <= max_bitrate))
            .cloned()
            .collect()
    }

    /// Get device model name
    pub fn get_device_model(&self) -> Result<String> {
        let mut cmd = self.adb_cmd(&["shell", "getprop", "ro.product.model"])?;
        let output = cmd.output().context("Failed to get device model")?;
        if output.status.success() {
            let model = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !model.is_empty() {
                return Ok(model);
            }
        }
        Ok("Unknown".to_string())
    }

    /// Get storage info
    pub fn get_storage_info(&self) -> Result<StorageInfo> {
        let mut cmd = self.adb_cmd(&["shell", "df", "/sdcard"])?;
        let output = cmd.output().context("Failed to get storage info")?;
        
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines().skip(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    let total = parts[1].parse::<u64>().unwrap_or(0) * 1024;
                    let _used = parts[2].parse::<u64>().unwrap_or(0) * 1024;
                    let free = parts[3].parse::<u64>().unwrap_or(0) * 1024;
                    return Ok(StorageInfo { total, free, available: free });
                }
            }
        }
        
        Ok(StorageInfo { total: 0, free: 0, available: 0 })
    }

    /// List directory entries (folders)
    pub fn list_directory(&self, dir: &str) -> Result<Vec<AdbDirEntry>> {
        let mut cmd = self.adb_cmd(&["shell", "ls", "-la", dir])?;
        let output = cmd.output().context("Failed to list directory")?;
        
        let mut entries = Vec::new();
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines().skip(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 9 && parts[0].starts_with('d') {
                    let name = parts[8..].join(" ");
                    if name != "." && name != ".." {
                        let mtime = Some(format!("{} {} {}", parts[5], parts[6], parts[7]));
                        entries.push(AdbDirEntry {
                            name: name.clone(),
                            path: format!("{}/{}", dir.trim_end_matches('/'), name),
                            mtime,
                        });
                    }
                }
            }
        }
        Ok(entries)
    }

    /// Push file to device
    pub fn push_file(&self, local: &PathBuf, remote: &Path, conflict: &str) -> Result<()> {
        let remote_str = remote.to_string_lossy().to_string();
        let remote_dir = remote.parent().unwrap_or(Path::new("/")).to_string_lossy().to_string();
        
        let mut cmd = self.adb_cmd(&["shell", "mkdir", "-p", &remote_dir])?;
        let _ = cmd.output();
        
        let remote_file = remote_str.clone();
        if conflict == "skip" {
            let mut check = self.adb_cmd(&["shell", "test", "-f", &remote_file])?;
            if check.output().map(|o| o.status.success()).unwrap_or(false) {
                return Err(anyhow::anyhow!("File already exists (skip mode)"));
            }
        } else if conflict == "overwrite" {
            let mut rm = self.adb_cmd(&["shell", "rm", "-f", &remote_file])?;
            let _ = rm.output();
        }
        
        let mut cmd = self.adb_cmd(&["push", local.to_str().unwrap(), &remote_str])?;
        let output = cmd.output().context("Failed to push file")?;
        
        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr);
            Err(anyhow::anyhow!("adb push failed: {}", err))
        }
    }

    /// Pull file from device
    pub fn pull_file(&self, remote: &Path, local: &PathBuf, conflict: &str) -> Result<()> {
        let remote_str = remote.to_string_lossy().to_string();
        let local_str = local.to_string_lossy().to_string();
        
        if conflict == "skip" && local.exists() {
            return Err(anyhow::anyhow!("Local file already exists (skip mode)"));
        } else if conflict == "overwrite" && local.exists() {
            let _ = std::fs::remove_file(local);
        }
        
        let mut cmd = self.adb_cmd(&["pull", &remote_str, &local_str])?;
        let output = cmd.output().context("Failed to pull file")?;
        
        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr);
            Err(anyhow::anyhow!("adb pull failed: {}", err))
        }
    }
}


pub fn build_search_query(song: &ScannedSong) -> String {
    let mut query = String::new();
    query.push_str(&song.title);
    if !song.artist.is_empty() && song.artist != "Unknown Artist" {
        query.push(' ');
        query.push_str(&song.artist);
    }
    query
}