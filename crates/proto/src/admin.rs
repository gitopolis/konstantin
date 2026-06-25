//! Admin control-plane wire types.
//!
//! These are separate from the public status socket's `Request` / `Response`
//! types. The transport is XPC, but the payload is still JSON so the Rust
//! daemon and tray can share one serde model.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MACH_SERVICE_NAME: &str = "com.gitopolis.screentimed.control";
pub const TRAY_SIGNING_IDENTIFIER: &str = "com.gitopolis.konstantin";

pub const KEY_VERSION: &str = "version";
pub const KEY_REQUEST_ID: &str = "request_id";
pub const KEY_OK: &str = "ok";
pub const KEY_PAYLOAD_JSON: &str = "payload_json";
pub const KEY_ERROR: &str = "error";

pub const PROTOCOL_VERSION: u64 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdminRequest {
    GetConfig,
    ValidateConfig { toml: String },
    SetConfig { toml: String },
    ReloadDaemon,
    GetDaemonInfo,
    GetEnforcementState,
    SetEnforcementPaused { paused: bool },
    PrepareUninstall { preserve_config: bool },
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
    DaemonInfo {
        version: String,
    },
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestEnvelope {
    pub version: u64,
    pub request_id: String,
    pub request: AdminRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseEnvelope {
    pub version: u64,
    pub request_id: String,
    pub response: AdminResponse,
}

impl RequestEnvelope {
    pub fn new(request_id: impl Into<String>, request: AdminRequest) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            request,
        }
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serializing admin XPC request envelope")
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let env: Self = serde_json::from_str(json).context("parsing admin XPC request envelope")?;
        ensure_protocol_version(env.version)?;
        Ok(env)
    }
}

impl ResponseEnvelope {
    pub fn new(request_id: impl Into<String>, response: AdminResponse) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            response,
        }
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serializing admin XPC response envelope")
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let env: Self =
            serde_json::from_str(json).context("parsing admin XPC response envelope")?;
        ensure_protocol_version(env.version)?;
        Ok(env)
    }
}

fn ensure_protocol_version(version: u64) -> Result<()> {
    if version != PROTOCOL_VERSION {
        anyhow::bail!("unsupported admin XPC protocol version {version}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_round_trips() {
        let env = RequestEnvelope::new("abc", AdminRequest::SetEnforcementPaused { paused: true });

        let json = env.to_json().unwrap();
        let decoded = RequestEnvelope::from_json(&json).unwrap();

        assert_eq!(decoded, env);
    }

    #[test]
    fn response_envelope_round_trips() {
        let env = ResponseEnvelope::new(
            "abc",
            AdminResponse::EnforcementState {
                paused: true,
                kill_switch_path: "/etc/screentimed/disable".into(),
            },
        );

        let json = env.to_json().unwrap();
        let decoded = ResponseEnvelope::from_json(&json).unwrap();

        assert_eq!(decoded, env);
    }

    #[test]
    fn rejects_unknown_protocol_version() {
        let json = r#"{"version":3,"request_id":"abc","request":{"kind":"get_config"}}"#;

        let err = RequestEnvelope::from_json(json).unwrap_err();

        assert!(err.to_string().contains("unsupported"));
    }
}
