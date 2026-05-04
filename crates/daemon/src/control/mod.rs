//! Admin control-plane primitives.
//!
//! This module intentionally starts transport-free. XPC should deserialize
//! into these request/response types, then call `Controller::handle`.

#![allow(dead_code)]

pub mod auth;

use crate::config::Config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdminRequest {
    GetConfig,
    ValidateConfig { toml: String },
    SetConfig { toml: String },
    ReloadDaemon,
    GetEnforcementState,
    SetEnforcementPaused { paused: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdminResponse {
    Config {
        toml: String,
        enforcement_paused: bool,
        kill_switch_path: PathBuf,
    },
    ValidationOk,
    EnforcementState {
        paused: bool,
        kill_switch_path: PathBuf,
    },
    Ok,
    Unauthorized {
        reason: String,
    },
    Error {
        message: String,
    },
}

pub struct Controller {
    config_path: PathBuf,
}

impl Controller {
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }

    pub fn handle(&self, operator: &auth::Operator, req: AdminRequest) -> AdminResponse {
        if !operator.allowed {
            return AdminResponse::Unauthorized {
                reason: operator.reason.clone(),
            };
        }

        match self.handle_authorized(req) {
            Ok(resp) => resp,
            Err(e) => AdminResponse::Error {
                message: e.to_string(),
            },
        }
    }

    fn handle_authorized(&self, req: AdminRequest) -> Result<AdminResponse> {
        match req {
            AdminRequest::GetConfig => {
                let toml = std::fs::read_to_string(&self.config_path).with_context(|| {
                    format!("reading config file {}", self.config_path.display())
                })?;
                let cfg = Config::parse_toml(&toml)?;
                Ok(AdminResponse::Config {
                    enforcement_paused: kill_switch_paused(&cfg)?,
                    kill_switch_path: cfg.kill_switch_path,
                    toml,
                })
            }
            AdminRequest::ValidateConfig { toml } => {
                Config::parse_toml(&toml)?;
                Ok(AdminResponse::ValidationOk)
            }
            AdminRequest::SetConfig { toml } => {
                let cfg = Config::parse_toml(&toml)?;
                write_config_atomic(&self.config_path, &toml)?;
                Ok(AdminResponse::EnforcementState {
                    paused: kill_switch_paused(&cfg)?,
                    kill_switch_path: cfg.kill_switch_path,
                })
            }
            AdminRequest::ReloadDaemon => {
                Config::load(&self.config_path)?;
                Ok(AdminResponse::Ok)
            }
            AdminRequest::GetEnforcementState => {
                let cfg = Config::load(&self.config_path)?;
                Ok(AdminResponse::EnforcementState {
                    paused: kill_switch_paused(&cfg)?,
                    kill_switch_path: cfg.kill_switch_path,
                })
            }
            AdminRequest::SetEnforcementPaused { paused } => {
                let cfg = Config::load(&self.config_path)?;
                set_enforcement_paused(&cfg, paused)?;
                Ok(AdminResponse::EnforcementState {
                    paused,
                    kill_switch_path: cfg.kill_switch_path,
                })
            }
        }
    }
}

fn kill_switch_paused(cfg: &Config) -> Result<bool> {
    validate_kill_switch_path(&cfg.kill_switch_path)?;
    Ok(cfg.kill_switch_path.exists())
}

fn set_enforcement_paused(cfg: &Config, paused: bool) -> Result<()> {
    let path = validate_kill_switch_path(&cfg.kill_switch_path)?;
    if paused {
        write_marker_atomic(path)
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
}

fn validate_kill_switch_path(path: &Path) -> Result<&Path> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("kill_switch_path must not be empty");
    }
    if !path.is_absolute() {
        anyhow::bail!("kill_switch_path must be absolute: {}", path.display());
    }
    if path.parent().is_none() {
        anyhow::bail!("kill_switch_path must have a parent: {}", path.display());
    }
    Ok(path)
}

fn write_marker_atomic(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("kill_switch_path unexpectedly has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating kill-switch dir {}", parent.display()))?;
    let tmp = path.with_extension(format!(
        "{}.tmp.{}",
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("marker"),
        std::process::id()
    ));
    std::fs::write(&tmp, b"disabled by Konstantin operator\n")
        .with_context(|| format!("writing {}", tmp.display()))?;
    set_mode_0600(&tmp)?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    set_mode_0600(path)?;
    Ok(())
}

fn write_config_atomic(path: &Path, toml: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
    }
    let tmp = path.with_extension(format!(
        "{}.tmp.{}",
        path.extension().and_then(|s| s.to_str()).unwrap_or("toml"),
        std::process::id()
    ));
    std::fs::write(&tmp, toml).with_context(|| format!("writing {}", tmp.display()))?;
    set_mode_0600(&tmp)?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    set_mode_0600(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 0600 {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn allowed_operator() -> auth::Operator {
        auth::Operator {
            uid: 501,
            username: "admin".into(),
            allowed: true,
            reason: "admin".into(),
        }
    }

    fn denied_operator() -> auth::Operator {
        auth::Operator {
            uid: 502,
            username: "standard".into(),
            allowed: false,
            reason: "not an administrator".into(),
        }
    }

    fn tempdir(name: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "screentimed-control-test-{name}-{n}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn config_text(dir: &Path) -> String {
        format!(
            r#"
socket_path = "{socket}"
state_path = "{state}"
tick_seconds = 5
default_policy = "unrestricted"
enforcement = "logout"
kill_switch_path = "{kill_switch}"

[users.alice]
daily_limit_minutes = 120
"#,
            socket = dir.join("screentimed.sock").display(),
            state = dir.join("state.json").display(),
            kill_switch = dir.join("disable").display(),
        )
    }

    #[test]
    fn denied_operator_cannot_read_config() {
        let dir = tempdir("denied");
        let path = dir.join("config.toml");
        std::fs::write(&path, config_text(&dir)).unwrap();
        let controller = Controller::new(path);

        let resp = controller.handle(&denied_operator(), AdminRequest::GetConfig);

        assert!(matches!(resp, AdminResponse::Unauthorized { .. }));
    }

    #[test]
    fn get_config_reports_enforcement_state() {
        let dir = tempdir("get-config");
        let path = dir.join("config.toml");
        std::fs::write(&path, config_text(&dir)).unwrap();
        let controller = Controller::new(path);

        let resp = controller.handle(&allowed_operator(), AdminRequest::GetConfig);

        match resp {
            AdminResponse::Config {
                toml,
                enforcement_paused,
                kill_switch_path,
            } => {
                assert!(toml.contains("[users.alice]"));
                assert!(!enforcement_paused);
                assert_eq!(kill_switch_path, dir.join("disable"));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn set_config_validates_and_writes_0600() {
        let dir = tempdir("set-config");
        let path = dir.join("config.toml");
        std::fs::write(&path, config_text(&dir)).unwrap();
        let controller = Controller::new(path.clone());
        let new_text =
            config_text(&dir).replace("daily_limit_minutes = 120", "daily_limit_minutes = 90");

        let resp = controller.handle(
            &allowed_operator(),
            AdminRequest::SetConfig { toml: new_text },
        );

        assert!(matches!(resp, AdminResponse::EnforcementState { .. }));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("daily_limit_minutes = 90"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_config_is_rejected_without_overwriting() {
        let dir = tempdir("invalid-config");
        let path = dir.join("config.toml");
        let original = config_text(&dir);
        std::fs::write(&path, &original).unwrap();
        let controller = Controller::new(path.clone());

        let resp = controller.handle(
            &allowed_operator(),
            AdminRequest::SetConfig {
                toml: "tick_seconds = 0".into(),
            },
        );

        assert!(matches!(resp, AdminResponse::Error { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn pause_and_unpause_enforcement_toggles_kill_switch() {
        let dir = tempdir("pause");
        let path = dir.join("config.toml");
        let kill_switch = dir.join("disable");
        std::fs::write(&path, config_text(&dir)).unwrap();
        let controller = Controller::new(path);

        let paused = controller.handle(
            &allowed_operator(),
            AdminRequest::SetEnforcementPaused { paused: true },
        );
        assert!(matches!(
            paused,
            AdminResponse::EnforcementState { paused: true, .. }
        ));
        assert!(kill_switch.exists());
        assert_eq!(
            std::fs::metadata(&kill_switch)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let unpaused = controller.handle(
            &allowed_operator(),
            AdminRequest::SetEnforcementPaused { paused: false },
        );
        assert!(matches!(
            unpaused,
            AdminResponse::EnforcementState { paused: false, .. }
        ));
        assert!(!kill_switch.exists());
    }

    #[test]
    fn relative_kill_switch_path_is_rejected() {
        let cfg = Config {
            kill_switch_path: PathBuf::from("relative-disable"),
            ..Config::parse_toml(&config_text(&tempdir("relative"))).unwrap()
        };

        let err = set_enforcement_paused(&cfg, true).unwrap_err();

        assert!(err.to_string().contains("must be absolute"));
    }
}
