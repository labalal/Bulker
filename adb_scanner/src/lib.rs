use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use rayon::prelude::*;

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

pub struct AdbScanner {
    device_id: Option<String>,
}

impl AdbScanner {
    pub fn new(device_id: Option<String>) -> Self {
        Self { device_id }
    }

    fn adb_cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("adb");
        if let Some(ref id) = self.device_id {
            cmd.args(["-s", id]);
        }
        cmd.args(args);
        cmd
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
                    Some(parts[0].to_string())
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

        let results: Vec<_> = music_dirs.par_iter()
            .filter_map(|dir| {
                match self.scan_directory(dir) {
                    Ok(songs) => {
                        println!("Found {} songs in {}", songs.len(), dir);
                        Some(songs)
                    }
                    Err(e) => {
                        eprintln!("Warning: Could not scan {}: {}", dir, e);
                        None
                    }
                }
            })
            .flatten()
            .collect();

        Ok(results)
    }

    fn scan_directory(&self, dir: &str) -> Result<Vec<ScannedSong>> {
        let extensions = ["opus", "mp3", "flac", "m4a", "ogg", "wav"];
        let mut all_files = Vec::new();
        
        for ext in extensions {
            let find_cmd = format!("ls {}/*.{} 2>/dev/null", dir, ext);
            let output = self.adb_cmd(&["shell", &find_cmd])
                .output()
                .context("Failed to run ls command")?;
            
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !trimmed.contains("No such file") {
                        all_files.push(trimmed.to_string());
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
        let output = self.adb_cmd(&[
            "shell",
            "mediainfo",
            "--Inform=General;%Title%|%Performer%|%Album%|%Duration%|%BitRate%",
            file_path
        ])
        .output()
        .context("Failed to run mediainfo")?;

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
        let output = self.adb_cmd(&["shell", "stat", "-c", "%s", file_path])
            .output()
            .context("Failed to get file size")?;

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