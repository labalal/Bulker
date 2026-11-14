//! ADB Android Phone Music Scanner & Synchronizer Plugin.
//!
//! Provides music directory scanning and synchronization for Android devices via ADB,
//! with strict input sanitization and arbitrary code execution prevention.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::json;
use std::process::Command;
use crate::plugin::{BulkerPlugin, DeviceScannedSong, PluginContext, ScannedDevice};
use crate::security::{validate_device_id, sanitize_filename};

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
            _ => bail!("Unknown command '{}' for ADB plugin", cmd),
        }
    }
}
