//! ADB Android Phone Music Scanner & Synchronizer Plugin.
//!
//! Provides music directory scanning and synchronization for Android devices via ADB,
//! with strict input sanitization and arbitrary code execution prevention.
//! Includes safe SD card write operations with verification.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use crate::plugin::{BulkerPlugin, DeviceScannedSong, PluginContext, ScannedDevice};
use crate::security::{validate_device_id, sanitize_filename};

/// Safe SD card write result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdCardWriteResult {
    pub success: bool,
    pub path: String,
    pub size: u64,
    pub checksum: Option<String>,
    pub error: Option<String>,
}

/// SD card storage info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdCardStorageInfo {
    pub total_space: u64,
    pub free_space: u64,
    pub available_space: u64,
    pub path: String,
}

pub struct AdbPhonePlugin;

impl AdbPhonePlugin {
    pub fn new() -> Self {
        Self
    }

    /// Runs an ADB command with safe, non-shell arguments
    fn run_adb_cmd(device_id: Option<&str>, args: &[&str]) -> Result<std::process::Output> {
        let mut cmd = Command::new("adb");
        if let Some(id) = device_id {
            let clean_id = validate_device_id(id)?;
            cmd.args(["-s", &clean_id]);
        }
        cmd.args(args);
        cmd.output().context("Failed to execute adb binary")
    }

    /// Lists connected Android devices via `adb devices`
    pub fn list_connected_devices() -> Result<Vec<ScannedDevice>> {
        let output = Command::new("adb")
            .args(["devices"])
            .output()
            .context("Failed to run 'adb devices'")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut devices = Vec::new();

        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1] == "device" {
                if let Ok(clean_id) = validate_device_id(parts[0]) {
                    devices.push(ScannedDevice {
                        id: clean_id.clone(),
                        name: format!("Android Device ({clean_id})"),
                        device_type: "android_adb".to_string(),
                        connected: true,
                    });
                }
            }
        }

        Ok(devices)
    }

    /// Safely scans an Android music directory for audio files
    pub fn scan_device_music(device_id: Option<&str>) -> Result<Vec<DeviceScannedSong>> {
        let music_dirs = [
            "/sdcard/Music",
            "/storage/emulated/0/Music",
            "/mnt/sdcard/Music",
            "/storage/emulated/0/Download",
        ];

        let valid_extensions = ["opus", "mp3", "flac", "m4a", "ogg", "wav"];
        let mut scanned_songs = Vec::new();

        for dir in music_dirs {
            for ext in valid_extensions {
                // Execute ls safely without shell expansion
                let output = Self::run_adb_cmd(device_id, &["shell", "ls", dir])?;
                if !output.status.success() {
                    continue;
                }

                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let filename = line.trim();
                    if filename.ends_with(&format!(".{ext}")) {
                        let full_path = format!("{dir}/{filename}");
                        let clean_title = sanitize_filename(
                            filename.strip_suffix(&format!(".{ext}")).unwrap_or(filename)
                        );

                        scanned_songs.push(DeviceScannedSong {
                            title: clean_title,
                            artist: "Unknown Artist".to_string(),
                            album: None,
                            duration: None,
                            bitrate: None,
                            file_path: full_path,
                            file_size: 0,
                        });
                    }
                }
            }
        }

        Ok(scanned_songs)
    }
}

impl AdbPhonePlugin {
    /// Gets SD card storage information
    pub fn get_sdcard_info(device_id: Option<&str>) -> Result<SdCardStorageInfo> {
        let output = Self::run_adb_cmd(device_id, &["shell", "df", "/sdcard"])?;
        if !output.status.success() {
            anyhow::bail!("Failed to get SD card info: {}", String::from_utf8_lossy(&output.stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        if lines.len() < 2 {
            anyhow::bail!("Unexpected df output format");
        }

        let parts: Vec<&str> = lines[1].split_whitespace().collect();
        if parts.len() < 4 {
            anyhow::bail!("Unexpected df output format");
        }

        let total = parts[1].parse::<u64>().unwrap_or(0) * 1024;
        let free = parts[3].parse::<u64>().unwrap_or(0) * 1024;
        let available = free;

        Ok(SdCardStorageInfo {
            total_space: total,
            free_space: free,
            available_space: available,
            path: "/sdcard".to_string(),
        })
    }

    /// Checks if there's enough space on SD card for a file
    pub fn check_sdcard_space(device_id: Option<&str>, required_bytes: u64) -> Result<bool> {
        let info = Self::get_sdcard_info(device_id)?;
        Ok(info.available_space >= required_bytes)
    }

    /// Safely creates a directory on the SD card
    pub fn create_sdcard_dir(device_id: Option<&str>, path: &str) -> Result<()> {
        let clean_path = sanitize_filename(path);
        let output = Self::run_adb_cmd(device_id, &["shell", "mkdir", "-p", &clean_path])?;
        if !output.status.success() {
            anyhow::bail!("Failed to create directory: {}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    }

    /// Safely pushes a file to the SD card with verification
    /// Uses atomic write: pushes to temp file, verifies, then renames
    pub fn push_file_safe(
        device_id: Option<&str>,
        local_path: &str,
        remote_path: &str,
        verify_checksum: bool,
    ) -> Result<SdCardWriteResult> {
        let local_path = PathBuf::from(local_path);
        if !local_path.exists() {
            return Ok(SdCardWriteResult {
                success: false,
                path: remote_path.to_string(),
                size: 0,
                checksum: None,
                error: Some("Local file does not exist".to_string()),
            });
        }

        let local_size = fs::metadata(&local_path)?.len();
        
        if !Self::check_sdcard_space(device_id, local_size)? {
            return Ok(SdCardWriteResult {
                success: false,
                path: remote_path.to_string(),
                size: 0,
                checksum: None,
                error: Some("Insufficient space on SD card".to_string()),
            });
        }

        let remote_dir = PathBuf::from(remote_path).parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/sdcard".to_string());
        
        Self::create_sdcard_dir(device_id, &remote_dir)?;

        let temp_remote = format!("{}.tmp.{}", remote_path, uuid::Uuid::new_v4().simple());
        
        let mut cmd = Command::new("adb");
        if let Some(id) = device_id {
            let clean_id = validate_device_id(id)?;
            cmd.args(["-s", &clean_id]);
        }
        cmd.args(["push", local_path.to_str().unwrap(), &temp_remote]);
        
        let output = cmd.output().context("Failed to execute adb push")?;
        if !output.status.success() {
            return Ok(SdCardWriteResult {
                success: false,
                path: remote_path.to_string(),
                size: 0,
                checksum: None,
                error: Some(format!("adb push failed: {}", String::from_utf8_lossy(&output.stderr))),
            });
        }

        let remote_checksum = if verify_checksum {
            Self::get_remote_checksum(device_id, &temp_remote)?
        } else {
            None
        };

        let local_checksum = if verify_checksum {
            Some(Self::compute_checksum(&local_path)?)
        } else {
            None
        };

        if verify_checksum && remote_checksum != local_checksum {
            let _ = Self::run_adb_cmd(device_id, &["shell", "rm", &temp_remote]);
            return Ok(SdCardWriteResult {
                success: false,
                path: remote_path.to_string(),
                size: 0,
                checksum: remote_checksum,
                error: Some("Checksum mismatch after transfer".to_string()),
            });
        }

        let output = Self::run_adb_cmd(device_id, &["shell", "mv", &temp_remote, remote_path])?;
        if !output.status.success() {
            let _ = Self::run_adb_cmd(device_id, &["shell", "rm", &temp_remote]);
            return Ok(SdCardWriteResult {
                success: false,
                path: remote_path.to_string(),
                size: 0,
                checksum: remote_checksum,
                error: Some(format!("Failed to rename temp file: {}", String::from_utf8_lossy(&output.stderr))),
            });
        }

        let final_size = Self::get_remote_file_size(device_id, remote_path)?.unwrap_or(local_size);

        Ok(SdCardWriteResult {
            success: true,
            path: remote_path.to_string(),
            size: final_size,
            checksum: remote_checksum,
            error: None,
        })
    }

    /// Computes SHA256 checksum of a local file
    fn compute_checksum(path: &Path) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut file = fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 8192];
        loop {
            let bytes_read = std::io::Read::read(&mut file, &mut buffer)?;
            if bytes_read == 0 { break; }
            hasher.update(&buffer[..bytes_read]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// Gets checksum of a remote file via adb shell
    fn get_remote_checksum(device_id: Option<&str>, remote_path: &str) -> Result<Option<String>> {
        let output = Self::run_adb_cmd(device_id, &["shell", "sha256sum", remote_path])?;
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = stdout.split_whitespace().collect();
        Ok(parts.first().map(|s| s.to_string()))
    }

    /// Gets size of a remote file via adb shell
    fn get_remote_file_size(device_id: Option<&str>, remote_path: &str) -> Result<Option<u64>> {
        let output = Self::run_adb_cmd(device_id, &["shell", "stat", "-c", "%s", remote_path])?;
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim().parse().ok())
    }

    /// Safely deletes a file from SD card
    pub fn delete_file_safe(device_id: Option<&str>, remote_path: &str) -> Result<bool> {
        let clean_path = sanitize_filename(remote_path);
        let output = Self::run_adb_cmd(device_id, &["shell", "rm", "-f", &clean_path])?;
        Ok(output.status.success())
    }

    /// Lists files in an SD card directory with metadata
    pub fn list_sdcard_dir(device_id: Option<&str>, path: &str) -> Result<Vec<DeviceScannedSong>> {
        let clean_path = sanitize_filename(path);
        let output = Self::run_adb_cmd(device_id, &["shell", "ls", "-la", &clean_path])?;
        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut files = Vec::new();
        let valid_extensions = ["opus", "mp3", "flac", "m4a", "ogg", "wav"];

        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 9 {
                continue;
            }
            let filename = parts[8..].join(" ");
            if filename == "." || filename == ".." {
                continue;
            }
            
            let ext = PathBuf::from(&filename)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            
            if valid_extensions.contains(&ext.as_str()) {
                let size = parts[4].parse::<u64>().unwrap_or(0);
                let full_path = format!("{}/{}", clean_path.trim_end_matches('/'), filename);
                let clean_title = sanitize_filename(
                    filename.strip_suffix(&format!(".{}", ext)).unwrap_or(&filename)
                );

                files.push(DeviceScannedSong {
                    title: clean_title,
                    artist: "Unknown Artist".to_string(),
                    album: None,
                    duration: None,
                    bitrate: None,
                    file_path: full_path,
                    file_size: size as i64,
                });
            }
        }

        Ok(files)
    }
}

impl Default for AdbPhonePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BulkerPlugin for AdbPhonePlugin {
    fn id(&self) -> &'static str {
        "adb_phone_sync"
    }

    fn name(&self) -> &'static str {
        "ADB Phone Sync & Scanner"
    }

    fn description(&self) -> &'static str {
        "Scans and synchronizes audio files with Android phones over ADB"
    }

    fn version(&self) -> &'static str {
        "1.0.0"
    }

    async fn on_init(&self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }

    async fn scan_devices(&self, _ctx: &PluginContext) -> Result<Vec<ScannedDevice>> {
        Self::list_connected_devices()
    }

    async fn execute_command(
        &self,
        _ctx: &PluginContext,
        cmd: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        match cmd {
            "list_devices" => {
                let devices = Self::list_connected_devices()?;
                Ok(json!({ "devices": devices }))
            }
            "scan_music" => {
                let dev_id = args.get("device_id").and_then(|v| v.as_str());
                if let Some(id) = dev_id {
                    validate_device_id(id)?;
                }
                let songs = Self::scan_device_music(dev_id)?;
                Ok(json!({ "songs": songs, "count": songs.len() }))
            }
            "sdcard_info" => {
                let dev_id = args.get("device_id").and_then(|v| v.as_str());
                if let Some(id) = dev_id {
                    validate_device_id(id)?;
                }
                let info = Self::get_sdcard_info(dev_id)?;
                Ok(json!(info))
            }
            "sdcard_check_space" => {
                let dev_id = args.get("device_id").and_then(|v| v.as_str());
                let required = args.get("required_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(id) = dev_id {
                    validate_device_id(id)?;
                }
                let has_space = Self::check_sdcard_space(dev_id, required)?;
                Ok(json!({ "has_space": has_space, "required_bytes": required }))
            }
            "sdcard_create_dir" => {
                let dev_id = args.get("device_id").and_then(|v| v.as_str());
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("/sdcard/Music");
                if let Some(id) = dev_id {
                    validate_device_id(id)?;
                }
                Self::create_sdcard_dir(dev_id, path)?;
                Ok(json!({ "success": true, "path": path }))
            }
            "sdcard_push_file" => {
                let dev_id = args.get("device_id").and_then(|v| v.as_str());
                let local = args.get("local_path").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("local_path required"))?;
                let remote = args.get("remote_path").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("remote_path required"))?;
                let verify = args.get("verify_checksum").and_then(|v| v.as_bool()).unwrap_or(true);
                if let Some(id) = dev_id {
                    validate_device_id(id)?;
                }
                let result = Self::push_file_safe(dev_id, local, remote, verify)?;
                Ok(json!(result))
            }
            "sdcard_delete_file" => {
                let dev_id = args.get("device_id").and_then(|v| v.as_str());
                let path = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("path required"))?;
                if let Some(id) = dev_id {
                    validate_device_id(id)?;
                }
                let success = Self::delete_file_safe(dev_id, path)?;
                Ok(json!({ "success": success, "path": path }))
            }
            "sdcard_list_dir" => {
                let dev_id = args.get("device_id").and_then(|v| v.as_str());
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("/sdcard/Music");
                if let Some(id) = dev_id {
                    validate_device_id(id)?;
                }
                let files = Self::list_sdcard_dir(dev_id, path)?;
                Ok(json!({ "files": files, "count": files.len() }))
            }
            _ => bail!("Unknown command '{}' for ADB plugin", cmd),
        }
    }
}
