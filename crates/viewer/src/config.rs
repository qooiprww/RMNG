//! The viewer's only persisted config: the server address (`host:port` for the
//! port-1 video/input/clipboard connection), stored in
//! `~/.config/rmng-viewer/config.json`.
//!
//! This is the source of truth, replacing the old `RMNG_VIDEO` env var: the
//! title-bar Settings button edits it at runtime and persists here. `RMNG_VIDEO`
//! only seeds the default the very first run (before any config file exists), so
//! existing setups keep working; once a config file is written it wins.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Fallback address when neither a config file nor `RMNG_VIDEO` provides one.
pub const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:9001";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_addr: String,
}

impl Default for Config {
    fn default() -> Self {
        // Seed from RMNG_VIDEO if present (legacy override), else the default.
        let server_addr = std::env::var("RMNG_VIDEO").unwrap_or_else(|_| DEFAULT_SERVER_ADDR.to_string());
        Config { server_addr }
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".config")
    });
    base.join("rmng-viewer").join("config.json")
}

/// Load the persisted config, falling back to defaults (which seed from
/// `RMNG_VIDEO`) when the file is absent or unreadable.
pub fn load() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("invalid config at {path:?}: {e}; using defaults");
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

pub fn save(config: &Config) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {dir:?}"))?;
    }
    let text = serde_json::to_string_pretty(config).context("serialize config")?;
    std::fs::write(&path, text).with_context(|| format!("write {path:?}"))
}
