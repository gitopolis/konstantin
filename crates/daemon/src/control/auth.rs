//! Operator authorization helpers for the admin control plane.

use anyhow::{Context, Result};
use nix::unistd::{Uid, User};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Operator {
    pub uid: u32,
    pub username: String,
    pub allowed: bool,
    pub reason: String,
}

#[allow(dead_code)] // Used once the XPC transport passes real peer EUIDs in.
pub fn operator_from_uid(uid: u32) -> Operator {
    match resolve_username(uid) {
        Ok(username) => {
            if uid == 0 {
                return Operator {
                    uid,
                    username,
                    allowed: true,
                    reason: "root".into(),
                };
            }
            match admin_members() {
                Ok(admins) if admins.contains(&username) => Operator {
                    uid,
                    username,
                    allowed: true,
                    reason: "admin group member".into(),
                },
                Ok(_) => Operator {
                    uid,
                    username,
                    allowed: false,
                    reason: "user is not in the local admin group".into(),
                },
                Err(e) => Operator {
                    uid,
                    username,
                    allowed: false,
                    reason: format!("could not read local admin group: {e}"),
                },
            }
        }
        Err(e) => Operator {
            uid,
            username: format!("uid:{uid}"),
            allowed: false,
            reason: e.to_string(),
        },
    }
}

#[allow(dead_code)] // Used by `operator_from_uid` when the transport is wired.
fn resolve_username(uid: u32) -> Result<String> {
    User::from_uid(Uid::from_raw(uid))
        .context("looking up uid")?
        .map(|u| u.name)
        .ok_or_else(|| anyhow::anyhow!("uid {uid} does not resolve to a local user"))
}

#[allow(dead_code)] // Used by `operator_from_uid` when the transport is wired.
fn admin_members() -> Result<HashSet<String>> {
    let output = Command::new("/usr/bin/dscl")
        .args([".", "-read", "/Groups/admin", "GroupMembership"])
        .output()
        .context("reading admin group membership")?;
    if !output.status.success() {
        anyhow::bail!(
            "dscl admin group lookup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(parse_admin_members(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_admin_members(stdout: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut in_members = false;
    for line in stdout.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("GroupMembership:") {
            in_members = true;
            out.extend(rest.split_whitespace().map(ToOwned::to_owned));
            continue;
        }
        if in_members {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.contains(':') {
                break;
            }
            out.extend(trimmed.split_whitespace().map(ToOwned::to_owned));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_members_inline() {
        let members = parse_admin_members("GroupMembership: root alice bob\n");

        assert!(members.contains("root"));
        assert!(members.contains("alice"));
        assert!(members.contains("bob"));
    }

    #[test]
    fn admin_members_wrapped() {
        let members = parse_admin_members("GroupMembership:\n alice\n bob\n");

        assert!(members.contains("alice"));
        assert!(members.contains("bob"));
    }

    #[test]
    fn admin_members_stops_at_next_attribute() {
        let members = parse_admin_members(
            "GeneratedUID: ignored\nGroupMembership:\n alice\nPassword: *\n bob\n",
        );

        assert!(members.contains("alice"));
        assert!(!members.contains("bob"));
    }

    #[test]
    fn admin_members_ignores_preamble() {
        let members = parse_admin_members("Preamble\nGroupMembership: nikita\n");

        assert_eq!(members, HashSet::from(["nikita".to_string()]));
    }
}
