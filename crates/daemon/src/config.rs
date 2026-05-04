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

    /// If this file exists, the daemon will NOT invoke `launchctl bootout`
    /// even when `enforcement = "logout"`. Lets the operator disable
    /// enforcement live without restarting (or reconfiguring) the daemon.
    #[serde(default = "default_kill_switch_path")]
    pub kill_switch_path: PathBuf,

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
    PathBuf::from(konstantin_proto::DEFAULT_SOCKET_PATH)
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

fn default_kill_switch_path() -> PathBuf {
    PathBuf::from("/etc/screentimed/disable")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Self::parse_toml(&text)
    }

    pub fn load_or_seed(path: &Path) -> Result<Self> {
        match Self::load(path) {
            Ok(cfg) => Ok(cfg),
            Err(e) if config_missing(path, &e) => {
                seed_default_config(path)?;
                Self::load(path)
            }
            Err(e) => Err(e),
        }
    }

    pub fn parse_toml(text: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(text).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub(crate) fn validate(&self) -> Result<()> {
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

fn config_missing(path: &Path, err: &anyhow::Error) -> bool {
    if path.exists() {
        return false;
    }
    err.chain()
        .filter_map(|e| e.downcast_ref::<std::io::Error>())
        .any(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn seed_default_config(path: &Path) -> Result<()> {
    let text = include_str!("../../../packaging/config.example.toml");
    SelfCheck::parse(text)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    set_mode_0600(path)?;
    Ok(())
}

struct SelfCheck;

impl SelfCheck {
    fn parse(text: &str) -> Result<()> {
        Config::parse_toml(text).map(|_| ())
    }
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 0600 {}", path.display()))
}

#[cfg(test)]
mod seed_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir(name: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "screentimed-config-test-{name}-{n}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn load_or_seed_creates_missing_config_0600() {
        let path = tempdir("seed").join("config.toml");

        let cfg = Config::load_or_seed(&path).unwrap();

        assert_eq!(cfg.default_policy, DefaultPolicy::Unrestricted);
        assert!(path.exists());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
