//! Daemon config: parsed from `/etc/screentimed/config.toml`.
//!
//! Example:
//!
//! ```toml
//! socket_path = "/var/run/screentimed.sock"
//! state_path  = "/var/db/screentimed/state.json"
//! tick_seconds = 5
//! warn_thresholds_minutes = [15, 5, 1]
//! default_policy = "unrestricted" # or "block"
//! enforcement = "log"             # or "logout"
//!
//! [users.alice]
//! daily_limit_minutes = 120
//!
//! [users.bob]
//! daily_limit_minutes = 240
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_socket_path")]
    pub socket_path: PathBuf,

    #[serde(default = "default_state_path")]
    pub state_path: PathBuf,

    #[serde(default = "default_tick_seconds")]
    pub tick_seconds: u32,

    #[serde(default = "default_warn_thresholds")]
    pub warn_thresholds_minutes: Vec<u32>,

    #[serde(default)]
    pub default_policy: DefaultPolicy,

    #[serde(default)]
    pub enforcement: Enforcement,

    #[serde(default)]
    pub users: HashMap<String, UserConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserConfig {
    pub daily_limit_minutes: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultPolicy {
    /// Users not in the config are unrestricted (logged but never kicked).
    #[default]
    Unrestricted,
    /// Users not in the config are immediately kicked. Use with care.
    Block,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// Log "would have kicked" but do nothing. Default — safe for early bring-up.
    #[default]
    Log,
    /// Actually run `launchctl bootout` when limit is reached.
    Logout,
}

fn default_socket_path() -> PathBuf {
    PathBuf::from(screentime_proto::DEFAULT_SOCKET_PATH)
}

fn default_state_path() -> PathBuf {
    PathBuf::from("/var/db/screentimed/state.json")
}

fn default_tick_seconds() -> u32 {
    5
}

fn default_warn_thresholds() -> Vec<u32> {
    vec![15, 5, 1]
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Self = toml::from_str(&text).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.tick_seconds == 0 {
            anyhow::bail!("tick_seconds must be > 0");
        }
        for (name, u) in &self.users {
            if u.daily_limit_minutes == 0 {
                anyhow::bail!("user {name}: daily_limit_minutes must be > 0");
            }
        }
        Ok(())
    }

    pub fn user_by_name(&self, name: &str) -> Option<&UserConfig> {
        self.users.get(name)
    }
}
