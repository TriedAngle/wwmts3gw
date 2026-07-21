use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tsclientlib::Identity;

use crate::ts3::WhisperScope;

/// Settings entered in the GUI, persisted as TOML in the platform config dir
/// (`~/.config/wwmts3gw` on Linux, `~/Library/Application Support/wwmts3gw`
/// on macOS, `%APPDATA%\wwmts3gw` on Windows). The CLI does not read this
/// file; its flags keep working as before.
///
/// Numeric ids are stored as strings because they back GUI text fields
/// verbatim; they are parsed when the connection starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: String,
    pub name: String,
    pub server_password: String,
    /// Selects whether `channel_name` or `channel_id` is used.
    pub use_channel_id: bool,
    pub channel_name: String,
    pub channel_id: String,
    pub channel_password: String,
    pub whisper_enabled: bool,
    pub whisper_group_id: String,
    /// Optional separate whisper group for zeal; empty = same as the jungle one.
    pub zeal_group_id: String,
    pub whisper_scope: WhisperScope,
    pub volume: f32,
    /// Paths overriding the built-in announcement clips; empty = built-in.
    pub clip_60: String,
    pub clip_40: String,
    pub clip_20: String,
    pub zeal_clip: String,
    /// Start-delay field of the timer panel, as MM:SS.
    pub start_delay: String,
    pub dark_mode: bool,
    /// Generated on first connect and reused, so the bot keeps a stable
    /// identity on the server across restarts.
    pub identity: Option<Identity>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: String::new(),
            name: "rust-mp3-bot".into(),
            server_password: String::new(),
            use_channel_id: false,
            channel_name: String::new(),
            channel_id: String::new(),
            channel_password: String::new(),
            whisper_enabled: false,
            whisper_group_id: String::new(),
            zeal_group_id: String::new(),
            whisper_scope: WhisperScope::AllChannels,
            volume: 1.0,
            clip_60: String::new(),
            clip_40: String::new(),
            clip_20: String::new(),
            zeal_clip: String::new(),
            start_delay: "0:30".into(),
            dark_mode: false,
            identity: None,
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wwmts3gw")
        .context("could not determine the user config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

impl Config {
    /// Loads the stored config; falls back to defaults if there is none yet
    /// or it cannot be read (the GUI then just starts with an empty form).
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(Some(config)) => config,
            Ok(None) => Self::default(),
            Err(err) => {
                tracing::warn!("failed to load config, using defaults: {err:#}");
                Self::default()
            }
        }
    }

    fn try_load() -> Result<Option<Self>> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config = toml::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(config))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("failed to serialize config")?;
        std::fs::write(&path, text)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}
