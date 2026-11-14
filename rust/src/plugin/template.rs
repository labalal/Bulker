//! Bulker Plugin Starter Template.
//!
//! Copy this file to create your own plugin for Bulker.
//! Examples of custom plugins:
//! - Cloud sync (Google Drive, Nextcloud, WebDAV)
//! - Media server sync (Plex, Jellyfin, Navidrome)
//! - Scrobbling (Last.fm, ListenBrainz)
//! - Custom hardware/device sync (DAP audio players, USB mass storage, Bluetooth)
//! - Lyrics and artwork enrichment

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use crate::db::SongRow;
use crate::plugin::{BulkerPlugin, PluginContext, ScannedDevice};

/// Example Plugin Skeleton
pub struct CustomTemplatePlugin {
    // Add custom plugin state, configuration, or clients here
}

impl CustomTemplatePlugin {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for CustomTemplatePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BulkerPlugin for CustomTemplatePlugin {
    fn id(&self) -> &'static str {
        "template_plugin"
    }

    fn name(&self) -> &'static str {
        "Custom Template Plugin"
    }

    fn description(&self) -> &'static str {
        "Template demonstrating how to extend Bulker with custom functionality"
    }

    fn version(&self) -> &'static str {
        "1.0.0"
    }

    /// Run any initialization logic when Bulker launches
    async fn on_init(&self, _ctx: &PluginContext) -> Result<()> {
        println!("[Template Plugin] Initialized successfully");
        Ok(())
    }

    /// Triggered automatically after an audio file is tagged and stored
    async fn on_song_downloaded(&self, _ctx: &PluginContext, song: &SongRow) -> Result<()> {
        println!("[Template Plugin] Detected newly downloaded song: {} by {}", song.title, song.artist);
        // Custom logic: push to cloud, trigger webhook, update external playlist, etc.
        Ok(())
    }

    /// Discover devices, network shares, or connected hardware
    async fn scan_devices(&self, _ctx: &PluginContext) -> Result<Vec<ScannedDevice>> {
        Ok(vec![
            ScannedDevice {
                id: "example_dev_1".to_string(),
                name: "Example Target Device".to_string(),
                device_type: "template".to_string(),
                connected: true,
            }
        ])
    }

    /// Handle custom actions invoked via REST API (/api/plugins/:id/command)
    async fn execute_command(
        &self,
        _ctx: &PluginContext,
        cmd: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        match cmd {
            "ping" => Ok(json!({ "status": "pong", "time": chrono::Utc::now().to_rfc3339() })),
            "custom_action" => {
                // Always sanitize user inputs from args!
                let param = args.get("param").and_then(|v| v.as_str()).unwrap_or("default");
                let clean_param = crate::security::sanitize_filename(param);
                Ok(json!({ "status": "success", "processed_param": clean_param }))
            }
            _ => anyhow::bail!("Unknown command: '{}'", cmd),
        }
    }

    /// Cleanup resources on server shutdown
    async fn on_shutdown(&self) -> Result<()> {
        println!("[Template Plugin] Shutting down cleanly");
        Ok(())
    }
}
