use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cli::RunArgs;

pub const DEFAULT_BT_CLASS: u32 = 0x240414;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub airplay_match: Option<String>,
    pub pin: String,
    pub bt_alias: String,
    pub bt_class: u32,
    pub discoverable: bool,
    pub hd_audio: bool,
    pub airplay_volume: f32,
    pub sync_bluetooth_volume: bool,
    pub render_delay_ms: u32,
    pub airplay_discovery_secs: u64,
    pub bluetooth_wait_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            airplay_match: None,
            pin: "3939".to_string(),
            bt_alias: "a2a2 Bridge".to_string(),
            bt_class: DEFAULT_BT_CLASS,
            discoverable: true,
            hd_audio: false,
            airplay_volume: 0.20,
            sync_bluetooth_volume: false,
            render_delay_ms: 350,
            airplay_discovery_secs: 5,
            bluetooth_wait_secs: 0,
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path.map(PathBuf::from).or_else(default_config_path) else {
            return Ok(Self::default());
        };

        if !path.exists() {
            return Ok(Self::default());
        }

        let data = fs::read_to_string(&path)
            .with_context(|| format!("read config file {}", path.display()))?;
        toml::from_str(&data).with_context(|| format!("parse config file {}", path.display()))
    }

    pub fn merge_run_args(mut self, args: RunArgs) -> Self {
        if let Some(value) = args.airplay_match {
            self.airplay_match = Some(value);
        }
        if let Some(value) = args.pin {
            self.pin = value;
        }
        if let Some(value) = args.bt_alias {
            self.bt_alias = value;
        }
        if let Some(value) = args.bt_class {
            self.bt_class = value;
        }
        if args.discoverable {
            self.discoverable = true;
        }
        if args.no_discoverable {
            self.discoverable = false;
        }
        if args.hd_audio {
            self.hd_audio = true;
        }
        if args.standard_audio {
            self.hd_audio = false;
        }
        if let Some(value) = args.render_delay_ms {
            self.render_delay_ms = value;
        }
        if let Some(value) = args.airplay_discovery_secs {
            self.airplay_discovery_secs = value;
        }
        if let Some(value) = args.bluetooth_wait_secs {
            self.bluetooth_wait_secs = value;
        }
        self
    }
}

fn default_config_path() -> Option<PathBuf> {
    let local = PathBuf::from("a2a2.toml");
    if local.exists() {
        return Some(local);
    }

    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let user_config = home.join(".config/a2a2/config.toml");
    if user_config.exists() {
        return Some(user_config);
    }

    let system_config = PathBuf::from("/etc/a2a2.toml");
    if system_config.exists() {
        return Some(system_config);
    }

    None
}
