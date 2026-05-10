//! Daemon-mediated update install support.
//!
//! The tray still owns the network-facing half of updates. This module owns
//! the root-only local handoff: verify the downloaded zip again, unpack into
//! a root-owned staging directory, then spawn a detached helper that survives
//! booting out the current daemon.

#![allow(dead_code)]

use anyhow::{Context, Result};
use konstantin_proto::admin::UpdateInstallResult;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SYSTEM_PLIST: &str = "/Library/LaunchDaemons/com.gitopolis.screentimed.plist";
const SOCKET_PATH: &str = "/var/run/screentimed.sock";
const BUNDLE_MARKER: &str = "/etc/screentimed/bundle_path";
const LEGACY_DAEMON: &str = "/usr/local/libexec/screentimed";
const UPDATE_ROOT: &str = "/var/tmp/konstantin-updates";
const LOG_PATH: &str = "/var/log/screentimed.log";

const TRAY_REL: &str = "Contents/MacOS/konstantin-tray";
const DAEMON_REL: &str = "Contents/Resources/screentimed";
const UPDATER_REL: &str = "Contents/Resources/konstantin-updater";
const PLIST_REL: &str = "Contents/Library/LaunchDaemons/com.gitopolis.screentimed.plist";
const INFO_PLIST_REL: &str = "Contents/Info.plist";

pub struct StartedUpdate {
    pub result_path: PathBuf,
    pub bundle_root: PathBuf,
}

pub fn start_update(
    zip_path: &Path,
    expected_version: &str,
    expected_sha256: &str,
) -> Result<StartedUpdate> {
    validate_sha256(expected_sha256)?;
    validate_version(expected_version)?;
    let source_zip = validate_source_zip(zip_path)?;
    let bundle_root = resolve_bundle_root()?;

    let work_dir = make_work_dir()?;
    let copied_zip = work_dir.join("update.zip");
    fs::copy(&source_zip, &copied_zip).with_context(|| {
        format!(
            "copying update zip {} to {}",
            source_zip.display(),
            copied_zip.display()
        )
    })?;
    set_mode(&copied_zip, 0o600)?;

    let actual_sha = sha256_of_file(&copied_zip)?;
    if actual_sha != expected_sha256.to_ascii_lowercase() {
        anyhow::bail!(
            "update zip sha256 mismatch: expected {}, got {}",
            expected_sha256,
            actual_sha
        );
    }

    let unpacked = work_dir.join("unpacked");
    fs::create_dir_all(&unpacked).with_context(|| format!("creating {}", unpacked.display()))?;
    unzip(&copied_zip, &unpacked)?;

    let staged_bundle = unpacked.join("Konstantin.app");
    validate_staged_bundle(&staged_bundle, expected_version)?;

    let helper_src = current_helper_path(&staged_bundle)?;
    let helper_copy = work_dir.join("konstantin-updater");
    fs::copy(&helper_src, &helper_copy).with_context(|| {
        format!(
            "copying updater helper {} to {}",
            helper_src.display(),
            helper_copy.display()
        )
    })?;
    set_mode(&helper_copy, 0o755)?;

    let result_path = work_dir.join("result.json");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .with_context(|| format!("opening {}", LOG_PATH))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("cloning {}", LOG_PATH))?;

    Command::new(&helper_copy)
        .arg("--staged-bundle")
        .arg(&staged_bundle)
        .arg("--dest-bundle")
        .arg(&bundle_root)
        .arg("--result")
        .arg(&result_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .with_context(|| format!("spawning updater helper {}", helper_copy.display()))?;

    Ok(StartedUpdate {
        result_path,
        bundle_root,
    })
}

pub fn updater_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let parsed = parse_updater_args(&args);
    let Some(result_path) = parsed.as_ref().ok().map(|a| a.result_path.clone()) else {
        eprintln!(
            "usage: konstantin-updater --staged-bundle PATH --dest-bundle PATH --result PATH"
        );
        return 64;
    };

    let result = match parsed {
        Ok(args) => match install_update(&args) {
            Ok(()) => UpdateInstallResult::Succeeded,
            Err(UpdateFailure::Classified { code, message }) => {
                UpdateInstallResult::Failed { code, message }
            }
            Err(UpdateFailure::Unexpected(e)) => UpdateInstallResult::Failed {
                code: 99,
                message: e.to_string(),
            },
        },
        Err(e) => UpdateInstallResult::Failed {
            code: 64,
            message: e.to_string(),
        },
    };

    if let Err(e) = write_result(&result_path, &result) {
        eprintln!(
            "could not write update result {}: {e}",
            result_path.display()
        );
        return 70;
    }

    match result {
        UpdateInstallResult::Succeeded => 0,
        UpdateInstallResult::Failed { code, .. } => code,
    }
}

struct UpdaterArgs {
    staged_bundle: PathBuf,
    dest_bundle: PathBuf,
    result_path: PathBuf,
}

enum UpdateFailure {
    Classified { code: i32, message: String },
    Unexpected(anyhow::Error),
}

impl From<anyhow::Error> for UpdateFailure {
    fn from(value: anyhow::Error) -> Self {
        Self::Unexpected(value)
    }
}

fn install_update(args: &UpdaterArgs) -> std::result::Result<(), UpdateFailure> {
    ensure_root()?;
    validate_absolute_existing_dir(&args.staged_bundle, "staged bundle")?;
    validate_absolute_parent(&args.dest_bundle, "destination bundle")?;

    let backup = backup_path(&args.dest_bundle)?;
    fs::rename(&args.dest_bundle, &backup).map_err(|e| UpdateFailure::Classified {
        code: 10,
        message: format!("couldn't move existing bundle aside: {e}"),
    })?;

    if let Err(e) = fs::rename(&args.staged_bundle, &args.dest_bundle) {
        let _ = fs::rename(&backup, &args.dest_bundle);
        return Err(UpdateFailure::Classified {
            code: 11,
            message: format!("couldn't move new bundle into place: {e}"),
        });
    }

    write_bundle_marker(&args.dest_bundle)?;
    bootout_system_daemon();
    wait_for_daemon_exit();

    if let Err(e) = install_daemon_binary(&args.dest_bundle) {
        restore_backup(&args.dest_bundle, &backup)?;
        return Err(UpdateFailure::Classified {
            code: 20,
            message: format!("could not install daemon binary: {e}"),
        });
    }

    if let Err(e) = install_legacy_plist(&args.dest_bundle) {
        restore_backup(&args.dest_bundle, &backup)?;
        return Err(UpdateFailure::Classified {
            code: 21,
            message: format!("could not install LaunchDaemon plist: {e}"),
        });
    }

    if let Err(e) = bootstrap_system_daemon() {
        restore_backup(&args.dest_bundle, &backup)?;
        return Err(UpdateFailure::Classified {
            code: 22,
            message: format!("launchctl bootstrap failed: {e}"),
        });
    }

    if !wait_for_socket(Duration::from_secs(20)) {
        restore_backup(&args.dest_bundle, &backup)?;
        return Err(UpdateFailure::Classified {
            code: 23,
            message: format!("new daemon did not become reachable on {SOCKET_PATH} within 20s"),
        });
    }

    remove_dir_if_exists(&backup).map_err(UpdateFailure::Unexpected)?;
    Ok(())
}

fn restore_backup(dest_bundle: &Path, backup: &Path) -> std::result::Result<(), UpdateFailure> {
    bootout_system_daemon();
    wait_for_daemon_exit();
    let _ = fs::remove_dir_all(dest_bundle);
    fs::rename(backup, dest_bundle).map_err(|e| UpdateFailure::Classified {
        code: 50,
        message: format!("rollback could not restore the previous bundle: {e}"),
    })?;
    let _ = install_daemon_binary(dest_bundle);
    let _ = install_legacy_plist(dest_bundle);
    let _ = bootstrap_system_daemon();
    Ok(())
}

fn parse_updater_args(args: &[String]) -> Result<UpdaterArgs> {
    let mut staged_bundle = None;
    let mut dest_bundle = None;
    let mut result_path = None;
    let mut i = 1;
    while i < args.len() {
        let value = args
            .get(i + 1)
            .with_context(|| format!("missing value for {}", args[i]))?;
        match args[i].as_str() {
            "--staged-bundle" => staged_bundle = Some(PathBuf::from(value)),
            "--dest-bundle" => dest_bundle = Some(PathBuf::from(value)),
            "--result" => result_path = Some(PathBuf::from(value)),
            other => anyhow::bail!("unknown argument {other}"),
        }
        i += 2;
    }
    Ok(UpdaterArgs {
        staged_bundle: staged_bundle.context("missing --staged-bundle")?,
        dest_bundle: dest_bundle.context("missing --dest-bundle")?,
        result_path: result_path.context("missing --result")?,
    })
}

fn validate_source_zip(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("update zip path must be absolute: {}", path.display());
    }
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("update zip path is not a file: {}", path.display());
    }
    Ok(path.to_path_buf())
}

fn validate_sha256(hex: &str) -> Result<()> {
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("expected_sha256 must be 64 hexadecimal characters");
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version.contains('/')
        || version.contains('\0')
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
    {
        anyhow::bail!("invalid expected_version: {version}");
    }
    Ok(())
}

fn resolve_bundle_root() -> Result<PathBuf> {
    if let Some(root) = infer_bundle_root_from_exe()? {
        return Ok(root);
    }

    let marker = fs::read_to_string(BUNDLE_MARKER)
        .with_context(|| format!("reading bundle marker {BUNDLE_MARKER}"))?;
    let root = PathBuf::from(marker.trim());
    validate_absolute_existing_dir(&root, "bundle marker path")?;
    Ok(root)
}

fn infer_bundle_root_from_exe() -> Result<Option<PathBuf>> {
    let exe = std::env::current_exe().context("reading current executable path")?;
    let Some(resources) = exe.parent() else {
        return Ok(None);
    };
    if resources.file_name().and_then(|s| s.to_str()) != Some("Resources") {
        return Ok(None);
    }
    let Some(contents) = resources.parent() else {
        return Ok(None);
    };
    if contents.file_name().and_then(|s| s.to_str()) != Some("Contents") {
        return Ok(None);
    }
    Ok(contents.parent().map(Path::to_path_buf))
}

fn make_work_dir() -> Result<PathBuf> {
    let root = Path::new(UPDATE_ROOT);
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    set_mode(root, 0o755)?;

    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before UNIX_EPOCH")?
        .as_nanos();
    let dir = root.join(format!("update-{}-{n}", std::process::id()));
    fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    set_mode(&dir, 0o755)?;
    Ok(dir)
}

fn unzip(zip: &Path, into: &Path) -> Result<()> {
    validate_zip_entries(zip)?;
    let status = Command::new("/usr/bin/unzip")
        .arg("-q")
        .arg(zip)
        .arg("-d")
        .arg(into)
        .status()
        .context("spawning unzip")?;
    if !status.success() {
        anyhow::bail!("unzip exited with {status}");
    }
    Ok(())
}

fn validate_zip_entries(zip: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/zipinfo")
        .arg("-1")
        .arg(zip)
        .output()
        .context("spawning zipinfo for archive entries")?;
    if !output.status.success() {
        anyhow::bail!(
            "zipinfo failed listing archive entries: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut count = 0usize;
    let mut has_bundle_root = false;
    for entry in String::from_utf8_lossy(&output.stdout).lines() {
        validate_zip_entry_path(entry)?;
        count += 1;
        if entry == "Konstantin.app/" || entry.starts_with("Konstantin.app/") {
            has_bundle_root = true;
        }
        if count > 20_000 {
            anyhow::bail!("update archive has too many entries");
        }
    }
    if count == 0 {
        anyhow::bail!("update archive is empty");
    }
    if !has_bundle_root {
        anyhow::bail!("update archive does not contain Konstantin.app");
    }

    let output = Command::new("/usr/bin/zipinfo")
        .arg("-l")
        .arg(zip)
        .output()
        .context("spawning zipinfo for archive metadata")?;
    if !output.status.success() {
        anyhow::bail!(
            "zipinfo failed reading archive metadata: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.as_bytes().first() == Some(&b'l') {
            anyhow::bail!("update archive contains a symbolic link entry");
        }
    }

    Ok(())
}

fn validate_zip_entry_path(entry: &str) -> Result<()> {
    if entry.is_empty() || entry.contains('\0') || entry.contains('\\') {
        anyhow::bail!("unsafe archive entry path: {entry:?}");
    }
    let path = Path::new(entry);
    if path.is_absolute() {
        anyhow::bail!("absolute archive entry path: {entry:?}");
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            anyhow::bail!("unsafe archive entry path: {entry:?}");
        }
    }
    Ok(())
}

fn validate_staged_bundle(staged: &Path, expected_version: &str) -> Result<()> {
    validate_absolute_existing_dir(staged, "staged bundle")?;
    for (rel, label) in [
        (TRAY_REL, "tray binary"),
        (DAEMON_REL, "daemon binary"),
        (UPDATER_REL, "updater helper"),
        (PLIST_REL, "LaunchDaemon plist"),
        (INFO_PLIST_REL, "Info.plist"),
    ] {
        let p = staged.join(rel);
        let meta =
            fs::metadata(&p).with_context(|| format!("{label} missing at {}", p.display()))?;
        if meta.len() == 0 {
            anyhow::bail!("{label} at {} is empty", p.display());
        }
    }
    let version = read_bundle_version(staged)?;
    if version != expected_version {
        anyhow::bail!("staged bundle version {version} did not match expected {expected_version}");
    }
    verify_codesign(staged)?;
    Ok(())
}

fn read_bundle_version(bundle: &Path) -> Result<String> {
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleShortVersionString"])
        .arg(bundle.join(INFO_PLIST_REL))
        .output()
        .context("spawning PlistBuddy for bundle version")?;
    if !output.status.success() {
        anyhow::bail!(
            "PlistBuddy failed reading bundle version: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn verify_codesign(bundle: &Path) -> Result<()> {
    let status = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(bundle)
        .status()
        .context("spawning codesign verify")?;
    if !status.success() {
        anyhow::bail!("codesign verification failed for {}", bundle.display());
    }
    Ok(())
}

fn current_helper_path(bundle_root: &Path) -> Result<PathBuf> {
    let helper = bundle_root.join(UPDATER_REL);
    if !helper.is_file() {
        anyhow::bail!("updater helper not found at {}", helper.display());
    }
    Ok(helper)
}

fn install_daemon_binary(bundle: &Path) -> Result<()> {
    let src = bundle.join(DAEMON_REL);
    let dst = Path::new(LEGACY_DAEMON);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    replace_file(&src, dst, 0o755)
        .with_context(|| format!("installing daemon binary to {LEGACY_DAEMON}"))
}

fn replace_file(src: &Path, dst: &Path, mode: u32) -> Result<()> {
    let parent = dst
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("destination has no parent")?;
    let name = dst
        .file_name()
        .and_then(|s| s.to_str())
        .context("destination has no file name")?;
    let tmp = parent.join(format!(".{name}.tmp.{}", std::process::id()));

    let _ = fs::remove_file(&tmp);
    fs::copy(src, &tmp)
        .with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
    set_mode(&tmp, mode)?;
    fs::rename(&tmp, dst)
        .with_context(|| format!("renaming {} to {}", tmp.display(), dst.display()))?;
    Ok(())
}

fn install_legacy_plist(bundle: &Path) -> Result<()> {
    let src = bundle.join(PLIST_REL);
    let dst = Path::new(SYSTEM_PLIST);
    fs::copy(&src, dst)
        .with_context(|| format!("installing LaunchDaemon plist to {SYSTEM_PLIST}"))?;
    set_mode(dst, 0o644)?;
    run_plistbuddy_ignore(dst, "Delete :BundleProgram");
    run_plistbuddy_ignore(dst, "Delete :ProgramArguments");
    run_plistbuddy(dst, "Add :ProgramArguments array")?;
    run_plistbuddy(
        dst,
        &format!("Add :ProgramArguments:0 string {LEGACY_DAEMON}"),
    )
}

fn run_plistbuddy(path: &Path, command: &str) -> Result<()> {
    let status = Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(command)
        .arg(path)
        .status()
        .with_context(|| format!("spawning PlistBuddy {command}"))?;
    if !status.success() {
        anyhow::bail!("PlistBuddy command failed: {command}");
    }
    Ok(())
}

fn run_plistbuddy_ignore(path: &Path, command: &str) {
    let _ = Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(command)
        .arg(path)
        .status();
}

fn write_bundle_marker(bundle: &Path) -> Result<()> {
    if let Some(parent) = Path::new(BUNDLE_MARKER).parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(BUNDLE_MARKER, format!("{}\n", bundle.display()))
        .with_context(|| format!("writing {BUNDLE_MARKER}"))
}

fn bootout_system_daemon() {
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", "system/com.gitopolis.screentimed"])
        .status();
}

fn bootstrap_system_daemon() -> Result<()> {
    let status = Command::new("/bin/launchctl")
        .args(["bootstrap", "system", SYSTEM_PLIST])
        .status()
        .context("spawning launchctl bootstrap")?;
    if !status.success() {
        anyhow::bail!("launchctl bootstrap exited with {status}");
    }
    Ok(())
}

fn wait_for_daemon_exit() {
    for _ in 0..40 {
        if !command_success("/usr/bin/pgrep", &["-x", "screentimed"]) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = Command::new("/usr/bin/pkill")
        .args(["-KILL", "-x", "screentimed"])
        .status();
    std::thread::sleep(Duration::from_millis(500));
}

fn wait_for_socket(timeout: Duration) -> bool {
    let deadline = SystemTime::now() + timeout;
    loop {
        if std::os::unix::net::UnixStream::connect(SOCKET_PATH).is_ok() {
            return true;
        }
        if SystemTime::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn command_success(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn backup_path(bundle: &Path) -> Result<PathBuf> {
    let name = bundle
        .file_name()
        .and_then(|s| s.to_str())
        .context("destination bundle has no file name")?;
    Ok(bundle.with_file_name(format!("{name}.update-backup-{}", std::process::id())))
}

fn validate_absolute_existing_dir(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{label} must be absolute: {}", path.display());
    }
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_dir() {
        anyhow::bail!("{label} is not a directory: {}", path.display());
    }
    Ok(())
}

fn validate_absolute_parent(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{label} must be absolute: {}", path.display());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("path has no parent")?;
    validate_absolute_existing_dir(parent, "destination parent")
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn write_result(path: &Path, result: &UpdateInstallResult) -> Result<()> {
    let json = serde_json::to_string_pretty(result).context("serializing update result")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    set_mode(&tmp, 0o644)?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

fn sha256_of_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

fn ensure_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("konstantin-updater must run as root");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_sha256_shape() {
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_sha256("xyz").is_err());
    }

    #[test]
    fn rejects_unsafe_versions() {
        assert!(validate_version("1.2.3").is_ok());
        assert!(validate_version("1.2.3-beta+1").is_ok());
        assert!(validate_version("../1.2.3").is_err());
        assert!(validate_version("").is_err());
    }

    #[test]
    fn parses_updater_args() {
        let args = vec![
            "konstantin-updater".to_string(),
            "--staged-bundle".to_string(),
            "/tmp/Konstantin.app".to_string(),
            "--dest-bundle".to_string(),
            "/Applications/Konstantin.app".to_string(),
            "--result".to_string(),
            "/tmp/result.json".to_string(),
        ];

        let parsed = parse_updater_args(&args).unwrap();

        assert_eq!(parsed.staged_bundle, PathBuf::from("/tmp/Konstantin.app"));
        assert_eq!(
            parsed.dest_bundle,
            PathBuf::from("/Applications/Konstantin.app")
        );
        assert_eq!(parsed.result_path, PathBuf::from("/tmp/result.json"));
    }

    #[test]
    fn replace_file_uses_sibling_temp_then_rename() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "konstantin-replace-file-test-{}-{n}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();

        let src = dir.join("src");
        let dst = dir.join("dst");
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();

        replace_file(&src, &dst, 0o755).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!dir
            .join(format!(".dst.tmp.{}", std::process::id()))
            .exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_unsafe_zip_entry_paths() {
        assert!(validate_zip_entry_path("Konstantin.app/Contents/Info.plist").is_ok());
        assert!(validate_zip_entry_path("/tmp/owned").is_err());
        assert!(validate_zip_entry_path("../owned").is_err());
        assert!(validate_zip_entry_path("Konstantin.app/../owned").is_err());
        assert!(validate_zip_entry_path(r"Konstantin.app\owned").is_err());
    }
}
