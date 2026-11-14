//! Bulker Plugin Connector & Container Architecture.
//!
//! Provides a modular, extensible plugin framework allowing external integrations
//! (phone sync, ADB scanner, Spotify import, Plex/Jellyfin sync, cloud backups)
//! without polluting the public Bulker core with personal or device-specific logic.

pub mod template;
pub mod adb_phone;
pub mod tui;

use anyhow::{bail, Result};
use async_trait::async_trait;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use crate::db::{Db, SongRow};

/// Information about a detected or connected device
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScannedDevice {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub connected: bool,
}

/// Metadata representation of a song scanned on an external device
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceScannedSong {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub bitrate: Option<i32>,
    pub file_path: String,
    pub file_size: i64,
}

/// Safe execution context passed to plugins
#[derive(Clone)]
pub struct PluginContext {
    pub db: Arc<Db>,
}

/// Trait that all Bulker plugins must implement.
#[async_trait]
pub trait BulkerPlugin: Send + Sync {
    /// Unique machine identifier for this plugin (e.g., "adb_phone_sync")
    fn id(&self) -> &'static str;

    /// Human-friendly display name (e.g., "ADB Android Phone Sync")
    fn name(&self) -> &'static str;

    /// Description of the plugin capabilities
    fn description(&self) -> &'static str;

    /// Semantic version of the plugin
    fn version(&self) -> &'static str;

    /// Lifecycle hook: Called when Bulker initializes
    async fn on_init(&self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }

    /// Lifecycle hook: Called whenever a song completes download & metadata tagging
    async fn on_song_downloaded(&self, _ctx: &PluginContext, _song: &SongRow) -> Result<()> {
        Ok(())
    }

    /// Device/hardware discovery hook: Scan for connected phones, disks, or services
    async fn scan_devices(&self, _ctx: &PluginContext) -> Result<Vec<ScannedDevice>> {
        Ok(Vec::new())
    }

    /// Execute a custom plugin command (invoked via REST API /api/plugins/:id/command)
    async fn execute_command(
        &self,
        _ctx: &PluginContext,
        _cmd: &str,
        _args: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        bail!("Command not supported by this plugin");
    }

    /// Lifecycle hook: Called during graceful application shutdown
    async fn on_shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// Plugin information returned by public REST endpoints
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub enabled: bool,
}

/// Thread-safe Plugin Container & Manager
pub struct PluginManager {
    plugins: RwLock<FxHashMap<String, Arc<dyn BulkerPlugin>>>,
    ctx: PluginContext,
}

impl PluginManager {
    pub fn new(db: Arc<Db>) -> Self {
        Self {
            plugins: RwLock::new(FxHashMap::default()),
            ctx: PluginContext { db },
        }
    }

    /// Registers a new plugin with the container
    pub fn register(&self, plugin: Arc<dyn BulkerPlugin>) {
        let id = plugin.id().to_string();
        self.plugins.write().insert(id, plugin);
    }

    /// Initializes all registered plugins
    pub async fn init_all(&self) {
        let plugins: Vec<Arc<dyn BulkerPlugin>> = {
            self.plugins.read().values().cloned().collect()
        };

        for plugin in plugins {
            if let Err(e) = plugin.on_init(&self.ctx).await {
                eprintln!("[Plugin Error] Failed to initialize plugin {}: {}", plugin.id(), e);
            }
        }
    }

    /// Notifies plugins that a song was downloaded
    pub async fn dispatch_song_downloaded(&self, song: &SongRow) {
        let plugins: Vec<Arc<dyn BulkerPlugin>> = {
            self.plugins.read().values().cloned().collect()
        };

        for plugin in plugins {
            if let Err(e) = plugin.on_song_downloaded(&self.ctx, song).await {
                eprintln!("[Plugin Error] on_song_downloaded failed in {}: {}", plugin.id(), e);
            }
        }
    }

    /// Lists metadata for all registered plugins
    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.plugins
            .read()
            .values()
            .map(|p| PluginInfo {
                id: p.id().to_string(),
                name: p.name().to_string(),
                description: p.description().to_string(),
                version: p.version().to_string(),
                enabled: true,
            })
            .collect()
    }

    /// Safely executes a command on a specific plugin with input sanitization
    pub async fn execute_command(&self, plugin_id: &str, cmd: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
        // Sanitize plugin_id and command to prevent injection
        let clean_id = crate::security::sanitize_cli_arg(plugin_id, false)?;
        let clean_cmd = crate::security::sanitize_cli_arg(cmd, false)?;

        let plugin = {
            self.plugins.read().get(&clean_id).cloned()
        };

        match plugin {
            Some(p) => p.execute_command(&self.ctx, &clean_cmd, args).await,
            None => bail!("Plugin '{}' not found", clean_id),
        }
    }
}
