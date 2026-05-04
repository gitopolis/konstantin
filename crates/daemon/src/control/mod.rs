//! Admin control-plane primitives.
//!
//! This module intentionally starts transport-free. XPC should deserialize
//! into these request/response types, then call `Controller::handle`.

#![allow(dead_code)]

pub mod auth;
pub mod xpc;

use crate::config::Config;
use anyhow::{Context, Result};
use konstantin_proto::admin::{
    AdminRequest, AdminResponse, TrayAutostartChange, TrayAutostartProbe, TrayAutostartState,
};
use std::path::{Path, PathBuf};
use std::process::Command;

const SYSTEM_CONFIG_PATH: &str = "/etc/screentimed/config.toml";
const TRAY_AGENT_LABEL: &str = "com.gitopolis.konstantin-tray";
const TRAY_AGENT_FILENAME: &str = "com.gitopolis.konstantin-tray.plist";

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

        match self.handle_authorized(operator, req) {
            Ok(resp) => resp,
            Err(e) => AdminResponse::Error {
                message: e.to_string(),
            },
        }
    }

    fn handle_authorized(
        &self,
        operator: &auth::Operator,
        req: AdminRequest,
    ) -> Result<AdminResponse> {
        match req {
            AdminRequest::GetConfig { autostart_probes } => {
                let toml = std::fs::read_to_string(&self.config_path).with_context(|| {
                    format!("reading config file {}", self.config_path.display())
                })?;
                let cfg = Config::parse_toml(&toml)?;
                Ok(AdminResponse::Config {
                    enforcement_paused: kill_switch_paused(&cfg)?,
                    kill_switch_path: cfg.kill_switch_path,
                    tray_autostart: tray_autostart_states(&autostart_probes),
                    toml,
                })
            }
            AdminRequest::ValidateConfig { toml } => {
                Config::parse_toml(&toml)?;
                Ok(AdminResponse::ValidationOk)
            }
            AdminRequest::SetConfig {
                toml,
                tray_exe,
                tray_autostart,
            } => {
                let cfg = Config::parse_toml(&toml)?;
                validate_tray_autostart_request(&tray_exe, &tray_autostart)?;
                write_config_atomic(&self.config_path, &toml)?;
                apply_tray_autostart_changes(&tray_exe, &tray_autostart)?;
                self.kickstart_daemon_after_reply();
                Ok(AdminResponse::EnforcementState {
                    paused: kill_switch_paused(&cfg)?,
                    kill_switch_path: cfg.kill_switch_path,
                })
            }
            AdminRequest::ReloadDaemon => {
                Config::load(&self.config_path)?;
                self.kickstart_daemon_after_reply();
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
            AdminRequest::Uninstall { preserve_config } => {
                if self.config_path != Path::new(SYSTEM_CONFIG_PATH) {
                    anyhow::bail!("uninstall is only available for the system daemon");
                }
                schedule_uninstall_after_reply(operator.uid, preserve_config);
                Ok(AdminResponse::Ok)
            }
        }
    }

    fn kickstart_daemon_after_reply(&self) {
        if self.config_path != Path::new(SYSTEM_CONFIG_PATH) {
            return;
        }
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            match Command::new("/bin/launchctl")
                .args(["kickstart", "-k", "system/com.gitopolis.screentimed"])
                .status()
            {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    tracing::warn!(%status, "failed to kickstart screentimed after config write");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to run launchctl kickstart after config write");
                }
            }
        });
    }
}

#[cfg(not(test))]
fn schedule_uninstall_after_reply(operator_uid: u32, preserve_config: bool) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        if let Err(e) = uninstall_system(operator_uid, preserve_config) {
            tracing::error!(error = %e, "daemon-mediated uninstall failed");
        }
    });
}

#[cfg(test)]
fn schedule_uninstall_after_reply(_operator_uid: u32, _preserve_config: bool) {}

fn uninstall_system(operator_uid: u32, preserve_config: bool) -> Result<()> {
    remove_file_if_exists(Path::new("/Library/LaunchDaemons/com.gitopolis.screentimed.plist"))?;
    remove_file_if_exists(Path::new("/Library/LaunchAgents/com.gitopolis.konstantin-tray.plist"))?;
    remove_file_if_exists(Path::new("/usr/local/libexec/screentimed"))?;
    remove_file_if_exists(Path::new("/usr/local/bin/konstantin-status"))?;
    remove_file_if_exists(Path::new("/usr/local/bin/konstantin-tray"))?;
    remove_file_if_exists(Path::new("/var/run/screentimed.sock"))?;
    remove_dir_if_exists(Path::new("/var/db/screentimed"))?;

    if preserve_config {
        remove_file_if_exists(Path::new("/etc/screentimed/bundle_path"))?;
    } else {
        remove_dir_if_exists(Path::new("/etc/screentimed"))?;
    }

    remove_user_tray_agents(operator_uid)?;
    bootout_system_daemon();
    Ok(())
}

fn remove_user_tray_agents(operator_uid: u32) -> Result<()> {
    let users_root = Path::new("/Users");
    let entries = match std::fs::read_dir(users_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", users_root.display())),
    };

    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry under {}", users_root.display()))?;
        let home = entry.path();
        if !home.is_dir() {
            continue;
        }
        let plist = tray_agent_path(&home);
        remove_file_if_exists(&plist)?;

        if let Ok(Some(uid)) = owner_uid(&home) {
            if uid != operator_uid {
                let username = entry.file_name().to_string_lossy().to_string();
                bootout_tray_agent(uid, &username);
            }
        }
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(unix)]
fn owner_uid(path: &Path) -> Result<Option<u32>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.uid())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

#[cfg(not(unix))]
fn owner_uid(_path: &Path) -> Result<Option<u32>> {
    Ok(None)
}

#[cfg(not(test))]
fn bootout_system_daemon() {
    match Command::new("/bin/launchctl")
        .args(["bootout", "system/com.gitopolis.screentimed"])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::debug!(%status, "screentimed bootout during uninstall was not successful");
        }
        Err(e) => {
            tracing::debug!(error = %e, "failed to run launchctl bootout for screentimed");
        }
    }
}

#[cfg(test)]
fn bootout_system_daemon() {}

fn tray_autostart_states(probes: &[TrayAutostartProbe]) -> Vec<TrayAutostartState> {
    probes
        .iter()
        .map(|probe| TrayAutostartState {
            username: probe.username.clone(),
            enabled: tray_agent_path(&probe.home).is_file(),
        })
        .collect()
}

fn validate_tray_autostart_request(
    tray_exe: &Path,
    changes: &[TrayAutostartChange],
) -> Result<()> {
    validate_tray_exe(tray_exe)?;
    for change in changes {
        validate_autostart_change(change)?;
    }
    Ok(())
}

fn apply_tray_autostart_changes(tray_exe: &Path, changes: &[TrayAutostartChange]) -> Result<()> {
    for change in changes {
        if change.enabled {
            enable_tray_autostart(tray_exe, change)?;
        } else {
            disable_tray_autostart(change)?;
        }
    }
    Ok(())
}

fn validate_autostart_change(change: &TrayAutostartChange) -> Result<()> {
    let username = change.username.trim();
    if username.is_empty() {
        anyhow::bail!("autostart change has empty username");
    }
    if username.contains('/') || username.contains('\0') {
        anyhow::bail!("autostart username contains invalid characters: {username}");
    }
    if change.uid == 0 {
        anyhow::bail!("refusing to manage tray autostart for root");
    }
    if !change.home.is_absolute() {
        anyhow::bail!("autostart home must be absolute: {}", change.home.display());
    }
    Ok(())
}

fn validate_tray_exe(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("tray executable path must be absolute: {}", path.display());
    }
    if !path.is_file() {
        anyhow::bail!("tray executable does not exist: {}", path.display());
    }
    Ok(())
}

fn enable_tray_autostart(tray_exe: &Path, change: &TrayAutostartChange) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let library_dir = change.home.join("Library");
    let agents_dir = library_dir.join("LaunchAgents");
    let logs_dir = library_dir.join("Logs");
    create_user_dir(&library_dir, change.uid, 0o700)?;
    create_user_dir(&agents_dir, change.uid, 0o755)?;
    create_user_dir(&logs_dir, change.uid, 0o700)?;

    let dst = tray_agent_path(&change.home);
    let body = build_user_launchagent_plist(tray_exe, &change.home);
    std::fs::write(&dst, body).with_context(|| format!("writing {}", dst.display()))?;
    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o644))
        .with_context(|| format!("chmod 0644 {}", dst.display()))?;
    std::os::unix::fs::chown(&dst, Some(change.uid), None)
        .with_context(|| format!("chown {} {}", change.uid, dst.display()))?;

    bootstrap_tray_agent(change.uid, &dst, &change.username);
    Ok(())
}

fn disable_tray_autostart(change: &TrayAutostartChange) -> Result<()> {
    let dst = tray_agent_path(&change.home);
    match std::fs::remove_file(&dst) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", dst.display())),
    }

    bootout_tray_agent(change.uid, &change.username);
    Ok(())
}

#[cfg(not(test))]
fn bootstrap_tray_agent(uid: u32, plist: &Path, username: &str) {
    let gui = format!("gui/{uid}");
    match Command::new("/bin/launchctl")
        .args(["bootstrap", &gui])
        .arg(plist)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::debug!(user = %username, %status, "tray LaunchAgent bootstrap was not successful");
        }
        Err(e) => {
            tracing::debug!(user = %username, error = %e, "failed to run launchctl bootstrap for tray LaunchAgent");
        }
    }
}

#[cfg(test)]
fn bootstrap_tray_agent(_uid: u32, _plist: &Path, _username: &str) {}

#[cfg(not(test))]
fn bootout_tray_agent(uid: u32, username: &str) {
    let service = format!("gui/{uid}/{TRAY_AGENT_LABEL}");
    match Command::new("/bin/launchctl")
        .args(["bootout", &service])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::debug!(user = %username, %status, "tray LaunchAgent bootout was not successful");
        }
        Err(e) => {
            tracing::debug!(user = %username, error = %e, "failed to run launchctl bootout for tray LaunchAgent");
        }
    }
}

#[cfg(test)]
fn bootout_tray_agent(_uid: u32, _username: &str) {}

fn create_user_dir(path: &Path, uid: u32, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))?;
    std::os::unix::fs::chown(path, Some(uid), None)
        .with_context(|| format!("chown {uid} {}", path.display()))?;
    Ok(())
}

fn tray_agent_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(TRAY_AGENT_FILENAME)
}

fn build_user_launchagent_plist(tray_exe: &Path, home: &Path) -> String {
    let exe = xml_escape(&tray_exe.display().to_string());
    let stdout = xml_escape(
        &home
            .join("Library/Logs/konstantin-tray.out.log")
            .display()
            .to_string(),
    );
    let stderr = xml_escape(
        &home
            .join("Library/Logs/konstantin-tray.err.log")
            .display()
            .to_string(),
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.gitopolis.konstantin-tray</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

        let resp = controller.handle(
            &denied_operator(),
            AdminRequest::GetConfig {
                autostart_probes: vec![],
            },
        );

        assert!(matches!(resp, AdminResponse::Unauthorized { .. }));
    }

    #[test]
    fn get_config_reports_enforcement_state() {
        let dir = tempdir("get-config");
        let path = dir.join("config.toml");
        std::fs::write(&path, config_text(&dir)).unwrap();
        let controller = Controller::new(path);

        let resp = controller.handle(
            &allowed_operator(),
            AdminRequest::GetConfig {
                autostart_probes: vec![TrayAutostartProbe {
                    username: "alice".into(),
                    home: dir.clone(),
                }],
            },
        );

        match resp {
            AdminResponse::Config {
                toml,
                enforcement_paused,
                kill_switch_path,
                tray_autostart,
            } => {
                assert!(toml.contains("[users.alice]"));
                assert!(!enforcement_paused);
                assert_eq!(kill_switch_path, dir.join("disable"));
                assert_eq!(tray_autostart.len(), 1);
                assert_eq!(tray_autostart[0].username, "alice");
                assert!(!tray_autostart[0].enabled);
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
            AdminRequest::SetConfig {
                toml: new_text,
                tray_exe: std::env::current_exe().unwrap(),
                tray_autostart: vec![],
            },
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
                tray_exe: std::env::current_exe().unwrap(),
                tray_autostart: vec![],
            },
        );

        assert!(matches!(resp, AdminResponse::Error { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn reload_validates_current_config() {
        let dir = tempdir("reload");
        let path = dir.join("config.toml");
        std::fs::write(&path, config_text(&dir)).unwrap();
        let controller = Controller::new(path);

        let resp = controller.handle(&allowed_operator(), AdminRequest::ReloadDaemon);

        assert!(matches!(resp, AdminResponse::Ok));
    }

    #[test]
    fn uninstall_is_system_daemon_only() {
        let dir = tempdir("uninstall-dev");
        let path = dir.join("config.toml");
        std::fs::write(&path, config_text(&dir)).unwrap();
        let controller = Controller::new(path);

        let resp = controller.handle(
            &allowed_operator(),
            AdminRequest::Uninstall {
                preserve_config: true,
            },
        );

        assert!(matches!(resp, AdminResponse::Error { .. }));
    }

    #[test]
    fn system_uninstall_schedules_after_reply() {
        let controller = Controller::new(PathBuf::from(SYSTEM_CONFIG_PATH));

        let resp = controller.handle(
            &allowed_operator(),
            AdminRequest::Uninstall {
                preserve_config: true,
            },
        );

        assert!(matches!(resp, AdminResponse::Ok));
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

    #[test]
    fn tray_autostart_enable_writes_user_owned_plist() {
        let dir = tempdir("autostart-enable");
        let exe = std::env::current_exe().unwrap();
        let change = TrayAutostartChange {
            username: "alice".into(),
            uid: unsafe { libc::getuid() },
            home: dir.clone(),
            enabled: true,
        };

        apply_tray_autostart_changes(&exe, &[change]).unwrap();

        let plist = tray_agent_path(&dir);
        let body = std::fs::read_to_string(&plist).unwrap();
        assert!(body.contains("com.gitopolis.konstantin-tray"));
        assert!(body.contains(&xml_escape(&exe.display().to_string())));
        assert_eq!(
            std::fs::metadata(&plist).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn tray_autostart_disable_removes_plist() {
        let dir = tempdir("autostart-disable");
        let plist = tray_agent_path(&dir);
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "placeholder").unwrap();
        let change = TrayAutostartChange {
            username: "alice".into(),
            uid: unsafe { libc::getuid() },
            home: dir,
            enabled: false,
        };

        apply_tray_autostart_changes(&std::env::current_exe().unwrap(), &[change]).unwrap();

        assert!(!plist.exists());
    }
}
