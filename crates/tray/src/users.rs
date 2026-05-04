//! Local-user enumeration for the Configure UI.
//!
//! Lists every "real" user account on the local Open Directory node:
//! `UniqueID >= 500`, name not starting with `_`, `/Users/<name>`
//! exists, and not in a small denylist (`nobody`/`daemon`/`root`/
//! `Guest`/`Shared`). Each entry carries the home directory,
//! admin-group membership flag, and (optionally) account picture
//! sourced from Open Directory's `Picture` attribute (file path) or
//! `JPEGPhoto` (raw JPEG bytes).
//!
//! Implementation sources data via `dscl(1)`. `-list /Users UniqueID`
//! and `-read /Groups/admin GroupMembership` go through the plain-text
//! output (small, line-oriented, easy to parse). Per-user attribute
//! reads use `-plist` so binary `JPEGPhoto` values come through cleanly
//! as `plist::Value::Data`.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct LocalUser {
    pub username: String,
    pub uid: u32,
    pub home: PathBuf,
    pub is_admin: bool,
    pub picture: Option<UserPicture>,
}

#[derive(Debug, Clone)]
pub enum UserPicture {
    /// `dsAttrTypeStandard:Picture` — usually a path under
    /// `/Library/User Pictures/`. The file is checked to exist before
    /// being returned.
    File(PathBuf),
    /// `dsAttrTypeStandard:JPEGPhoto` — raw JPEG bytes for custom
    /// photos a user uploaded.
    Jpeg(Vec<u8>),
}

/// Enumerate real local users on this Mac. Sorted by username.
pub fn enumerate() -> Result<Vec<LocalUser>> {
    let candidates = list_users()?;
    let admins = read_admin_members().unwrap_or_default();

    let mut users: Vec<LocalUser> = Vec::with_capacity(candidates.len());
    for (username, uid) in candidates {
        if !is_real_user(&username, uid) {
            continue;
        }
        let home_guess = Path::new("/Users").join(&username);
        if !home_guess.is_dir() {
            continue;
        }
        match read_user_attributes(&username) {
            Ok(attrs) => users.push(LocalUser {
                is_admin: admins.contains(&username),
                username,
                uid,
                home: attrs.home.unwrap_or(home_guess),
                picture: attrs.picture,
            }),
            Err(e) => {
                tracing::warn!(user = %username, error = %e, "skipping user (dscl read failed)");
            }
        }
    }
    users.sort_by(|a, b| a.username.cmp(&b.username));
    Ok(users)
}

fn list_users() -> Result<Vec<(String, u32)>> {
    let output = Command::new("/usr/bin/dscl")
        .args([".", "-list", "/Users", "UniqueID"])
        .output()
        .context("running `dscl . -list /Users UniqueID`")?;
    if !output.status.success() {
        anyhow::bail!(
            "dscl list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(parse_list_users(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_list_users(stdout: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut iter = line.split_whitespace();
        let name = match iter.next() {
            Some(n) => n,
            None => continue,
        };
        let uid_str = match iter.next() {
            Some(u) => u,
            None => continue,
        };
        if let Ok(uid) = uid_str.parse::<i64>() {
            if uid >= 0 && uid <= u32::MAX as i64 {
                out.push((name.to_string(), uid as u32));
            }
        }
    }
    out
}

const DENYLIST: &[&str] = &["nobody", "daemon", "root", "Guest", "Shared"];

fn is_real_user(name: &str, uid: u32) -> bool {
    if uid < 500 {
        return false;
    }
    if name.starts_with('_') {
        return false;
    }
    if DENYLIST.contains(&name) {
        return false;
    }
    true
}

fn read_admin_members() -> Result<HashSet<String>> {
    let output = Command::new("/usr/bin/dscl")
        .args([".", "-read", "/Groups/admin", "GroupMembership"])
        .output()
        .context("reading admin group membership")?;
    if !output.status.success() {
        return Ok(HashSet::new());
    }
    Ok(parse_admin_members(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_admin_members(stdout: &str) -> HashSet<String> {
    // Two formats observed:
    //   single-line: `GroupMembership: alice bob`
    //   wrapped:     `GroupMembership:\n alice\n bob`
    // Lines before the header are ignored — guards against `dscl`
    // surfacing diagnostic preamble.
    let mut members = HashSet::new();
    let mut in_section = false;
    for line in stdout.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("GroupMembership:") {
            in_section = true;
            for tok in rest.split_whitespace() {
                members.insert(tok.to_string());
            }
        } else if in_section {
            for tok in line.split_whitespace() {
                members.insert(tok.to_string());
            }
        }
    }
    members
}

#[derive(Default)]
struct UserAttributes {
    home: Option<PathBuf>,
    picture: Option<UserPicture>,
}

fn read_user_attributes(username: &str) -> Result<UserAttributes> {
    let user_path = format!("/Users/{}", username);
    let output = Command::new("/usr/bin/dscl")
        .args([
            "-plist",
            ".",
            "-read",
            &user_path,
            "NFSHomeDirectory",
            "Picture",
            "JPEGPhoto",
        ])
        .output()
        .with_context(|| format!("dscl read for {username}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "dscl -plist read failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    parse_user_plist(&output.stdout)
}

fn parse_user_plist(bytes: &[u8]) -> Result<UserAttributes> {
    use plist::Value;
    let value =
        Value::from_reader(std::io::Cursor::new(bytes)).context("parsing dscl -plist output")?;
    let dict = value
        .as_dictionary()
        .context("dscl plist root is not a dict")?;

    let home = dict
        .get("dsAttrTypeStandard:NFSHomeDirectory")
        .and_then(first_string)
        .map(PathBuf::from);

    // Custom JPEG photo wins over the legacy file-path Picture: macOS
    // System Settings prefers the user's uploaded photo when both
    // exist.
    let picture = jpeg_from_dict(dict).or_else(|| picture_path_from_dict(dict));

    Ok(UserAttributes { home, picture })
}

fn first_string(v: &plist::Value) -> Option<String> {
    let arr = v.as_array()?;
    arr.iter()
        .find_map(|item| item.as_string().map(|s| s.to_string()))
}

fn jpeg_from_dict(dict: &plist::Dictionary) -> Option<UserPicture> {
    let v = dict.get("dsAttrTypeStandard:JPEGPhoto")?;
    let arr = v.as_array()?;
    // dscl is inconsistent: even with `-plist` it emits binary
    // attributes as a whitespace-formatted hex `<string>`, e.g.
    //   <string>ffd8ffe0 00104a46 4946...</string>
    // rather than as a base64 `<data>` element. Accept both shapes.
    let bytes = arr.iter().find_map(|item| {
        if let plist::Value::Data(b) = item {
            return Some(b.clone());
        }
        if let Some(s) = item.as_string() {
            return hex_decode(s);
        }
        None
    })?;
    if bytes.is_empty() {
        None
    } else {
        Some(UserPicture::Jpeg(bytes))
    }
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let stripped: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if stripped.is_empty() || stripped.len() % 2 != 0 {
        return None;
    }
    let bytes = stripped.as_bytes();
    let mut out = Vec::with_capacity(stripped.len() / 2);
    for pair in bytes.chunks(2) {
        let s = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(out)
}

fn picture_path_from_dict(dict: &plist::Dictionary) -> Option<UserPicture> {
    let raw = dict
        .get("dsAttrTypeStandard:Picture")
        .and_then(first_string)?;
    let path = raw.trim();
    if path.is_empty() {
        return None;
    }
    let pb = PathBuf::from(path);
    if pb.is_file() {
        Some(UserPicture::File(pb))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_skips_blank_and_negative_uids() {
        let stdout = "
nobody                  -2
root                    0
_assetcache             224
alice                   501

bob                     502
";
        let parsed = parse_list_users(stdout);
        assert_eq!(
            parsed,
            vec![
                ("root".into(), 0),
                ("_assetcache".into(), 224),
                ("alice".into(), 501),
                ("bob".into(), 502),
            ]
        );
    }

    #[test]
    fn parse_list_handles_variable_padding() {
        let stdout = "alice 501\nbob\t\t502\n";
        let parsed = parse_list_users(stdout);
        assert_eq!(parsed, vec![("alice".into(), 501), ("bob".into(), 502)]);
    }

    #[test]
    fn real_user_filters() {
        assert!(!is_real_user("root", 0));
        assert!(!is_real_user("_assetcache", 224));
        assert!(!is_real_user("Guest", 501));
        assert!(!is_real_user("Shared", 501));
        assert!(is_real_user("alice", 501));
        assert!(is_real_user("nikita", 503));
    }

    #[test]
    fn admin_members_inline() {
        let m = parse_admin_members("GroupMembership: root alice\n");
        assert_eq!(m.len(), 2);
        assert!(m.contains("root"));
        assert!(m.contains("alice"));
    }

    #[test]
    fn admin_members_wrapped() {
        let m = parse_admin_members("GroupMembership:\n alice\n bob\n");
        assert_eq!(m.len(), 2);
        assert!(m.contains("alice"));
        assert!(m.contains("bob"));
    }

    #[test]
    fn admin_members_empty_section() {
        let m = parse_admin_members("GroupMembership:\n");
        assert!(m.is_empty());
    }

    #[test]
    fn admin_members_ignores_preamble() {
        // Defensive: a stray diagnostic line should not be parsed as a
        // member name.
        let m = parse_admin_members("Preamble blah\nGroupMembership: alice\n");
        assert_eq!(m, HashSet::from(["alice".into()]));
    }

    #[test]
    fn user_plist_parses_all_attrs() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>dsAttrTypeStandard:NFSHomeDirectory</key>
    <array>
        <string>/Users/alice</string>
    </array>
    <key>dsAttrTypeStandard:Picture</key>
    <array>
        <string>/path/that/does/not/exist.png</string>
    </array>
</dict>
</plist>
"#;
        let attrs = parse_user_plist(xml.as_bytes()).unwrap();
        assert_eq!(attrs.home, Some(PathBuf::from("/Users/alice")));
        // Picture path doesn't exist on test runner — should yield None,
        // not crash.
        assert!(attrs.picture.is_none());
    }

    #[test]
    fn user_plist_decodes_jpeg_as_data_element() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>dsAttrTypeStandard:JPEGPhoto</key>
    <array>
        <data>/9j/AA==</data>
    </array>
</dict>
</plist>
"#;
        let attrs = parse_user_plist(xml.as_bytes()).unwrap();
        match attrs.picture {
            Some(UserPicture::Jpeg(bytes)) => assert_eq!(bytes, vec![0xff, 0xd8, 0xff, 0x00]),
            other => panic!("expected Jpeg variant, got {other:?}"),
        }
    }

    #[test]
    fn user_plist_decodes_jpeg_as_hex_string() {
        // Real-world `dscl -plist` shape: binary attrs come back as a
        // hex-encoded <string>, not <data>.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>dsAttrTypeStandard:JPEGPhoto</key>
    <array>
        <string>ffd8ffe0 00104a46 49460001</string>
    </array>
</dict>
</plist>
"#;
        let attrs = parse_user_plist(xml.as_bytes()).unwrap();
        match attrs.picture {
            Some(UserPicture::Jpeg(bytes)) => {
                assert_eq!(
                    bytes,
                    vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01]
                );
            }
            other => panic!("expected Jpeg variant, got {other:?}"),
        }
    }

    #[test]
    fn hex_decode_handles_whitespace() {
        assert_eq!(hex_decode("ff d8 ff").unwrap(), vec![0xff, 0xd8, 0xff]);
        assert_eq!(hex_decode("ffd8ff"), Some(vec![0xff, 0xd8, 0xff]));
        assert_eq!(hex_decode(""), None);
        assert_eq!(hex_decode("abc"), None); // odd length
        assert_eq!(hex_decode("zz"), None);
    }
}
