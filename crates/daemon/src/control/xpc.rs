//! XPC admin-control transport scaffolding.
//!
//! The live listener/client wiring comes next. This file establishes the
//! stable Mach service name, dictionary keys, and serde envelope used across
//! the daemon and tray side of the transport.

#![allow(dead_code)]

use super::{auth, AdminRequest, AdminResponse, Controller};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const MACH_SERVICE_NAME: &str = "com.gitopolis.screentimed.control";
pub const TRAY_SIGNING_IDENTIFIER: &str = "com.gitopolis.konstantin";

pub const KEY_VERSION: &str = "version";
pub const KEY_REQUEST_ID: &str = "request_id";
pub const KEY_OK: &str = "ok";
pub const KEY_PAYLOAD_JSON: &str = "payload_json";
pub const KEY_ERROR: &str = "error";

pub const PROTOCOL_VERSION: u64 = 1;

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

pub fn handle_request_json(
    controller: &Controller,
    operator: &auth::Operator,
    request_json: &str,
) -> String {
    let response = match RequestEnvelope::from_json(request_json) {
        Ok(env) => {
            let response = controller.handle(operator, env.request);
            ResponseEnvelope::new(env.request_id, response)
        }
        Err(e) => ResponseEnvelope::new(
            "",
            AdminResponse::Error {
                message: e.to_string(),
            },
        ),
    };

    match response.to_json() {
        Ok(json) => json,
        Err(e) => {
            let fallback = ResponseEnvelope::new(
                "",
                AdminResponse::Error {
                    message: format!("serializing admin XPC response envelope: {e}"),
                },
            );
            serde_json::to_string(&fallback).unwrap_or_else(|_| {
                r#"{"version":1,"request_id":"","response":{"kind":"error","message":"fatal response serialization error"}}"#.to_string()
            })
        }
    }
}

fn ensure_protocol_version(version: u64) -> Result<()> {
    if version != PROTOCOL_VERSION {
        anyhow::bail!("unsupported admin XPC protocol version {version}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub mod ffi {
    use std::ffi::{c_char, c_void};

    pub type XpcObject = *mut c_void;
    pub type XpcConnection = XpcObject;
    pub type DispatchQueue = *mut c_void;

    pub const XPC_CONNECTION_MACH_SERVICE_LISTENER: u64 = 1 << 0;

    extern "C" {
        pub fn xpc_connection_create_mach_service(
            name: *const c_char,
            targetq: DispatchQueue,
            flags: u64,
        ) -> XpcConnection;

        pub fn xpc_connection_set_peer_team_identity_requirement(
            connection: XpcConnection,
            signing_identifier: *const c_char,
        ) -> i32;

        pub fn xpc_connection_get_euid(connection: XpcConnection) -> u32;

        pub fn xpc_connection_activate(connection: XpcConnection);
        pub fn xpc_connection_cancel(connection: XpcConnection);

        pub fn xpc_connection_send_message_with_reply_sync(
            connection: XpcConnection,
            message: XpcObject,
        ) -> XpcObject;

        pub fn xpc_dictionary_create_empty() -> XpcObject;
        pub fn xpc_dictionary_create_reply(original: XpcObject) -> XpcObject;
        pub fn xpc_dictionary_set_bool(xdict: XpcObject, key: *const c_char, value: bool);
        pub fn xpc_dictionary_get_bool(xdict: XpcObject, key: *const c_char) -> bool;
        pub fn xpc_dictionary_set_string(
            xdict: XpcObject,
            key: *const c_char,
            value: *const c_char,
        );
        pub fn xpc_dictionary_get_string(xdict: XpcObject, key: *const c_char) -> *const c_char;

        pub fn xpc_release(object: XpcObject);
    }
}

#[cfg(target_os = "macos")]
pub mod dictionary {
    use super::ffi;
    use anyhow::{Context, Result};
    use std::ffi::{CStr, CString};
    use std::ptr;

    pub struct OwnedObject {
        raw: ffi::XpcObject,
    }

    impl OwnedObject {
        pub fn empty_dictionary() -> Result<Self> {
            let raw = unsafe { ffi::xpc_dictionary_create_empty() };
            if raw.is_null() {
                anyhow::bail!("xpc_dictionary_create_empty returned NULL");
            }
            Ok(Self { raw })
        }

        pub fn raw(&self) -> ffi::XpcObject {
            self.raw
        }

        pub fn set_string(&self, key: &str, value: &str) -> Result<()> {
            let key = cstring(key, "dictionary key")?;
            let value = cstring(value, "dictionary string value")?;
            unsafe {
                ffi::xpc_dictionary_set_string(self.raw, key.as_ptr(), value.as_ptr());
            }
            Ok(())
        }

        pub fn get_string(&self, key: &str) -> Result<Option<String>> {
            let key = cstring(key, "dictionary key")?;
            let ptr = unsafe { ffi::xpc_dictionary_get_string(self.raw, key.as_ptr()) };
            if ptr.is_null() {
                return Ok(None);
            }
            let value = unsafe { CStr::from_ptr(ptr) }
                .to_str()
                .context("XPC string was not UTF-8")?
                .to_string();
            Ok(Some(value))
        }

        pub fn set_bool(&self, key: &str, value: bool) -> Result<()> {
            let key = cstring(key, "dictionary key")?;
            unsafe {
                ffi::xpc_dictionary_set_bool(self.raw, key.as_ptr(), value);
            }
            Ok(())
        }

        pub fn get_bool(&self, key: &str) -> Result<bool> {
            let key = cstring(key, "dictionary key")?;
            Ok(unsafe { ffi::xpc_dictionary_get_bool(self.raw, key.as_ptr()) })
        }
    }

    impl Drop for OwnedObject {
        fn drop(&mut self) {
            if !self.raw.is_null() {
                unsafe {
                    ffi::xpc_release(self.raw);
                }
                self.raw = ptr::null_mut();
            }
        }
    }

    fn cstring(value: &str, what: &str) -> Result<CString> {
        CString::new(value).with_context(|| format!("{what} contains an interior NUL byte"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
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
            reason: "user is not in the local admin group".into(),
        }
    }

    fn tempdir(name: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "screentimed-xpc-test-{name}-{n}-{}",
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
        let json = r#"{"version":2,"request_id":"abc","request":{"kind":"get_config"}}"#;

        let err = RequestEnvelope::from_json(json).unwrap_err();

        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn dispatches_request_json_through_controller() {
        let dir = tempdir("dispatch");
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, config_text(&dir)).unwrap();
        let controller = Controller::new(config_path);
        let request = RequestEnvelope::new("abc", AdminRequest::GetEnforcementState)
            .to_json()
            .unwrap();

        let response_json = handle_request_json(&controller, &allowed_operator(), &request);
        let response = ResponseEnvelope::from_json(&response_json).unwrap();

        assert_eq!(response.request_id, "abc");
        assert_eq!(
            response.response,
            AdminResponse::EnforcementState {
                paused: false,
                kill_switch_path: dir.join("disable"),
            }
        );
    }

    #[test]
    fn dispatch_preserves_unauthorized_response() {
        let dir = tempdir("unauthorized");
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, config_text(&dir)).unwrap();
        let controller = Controller::new(config_path);
        let request = RequestEnvelope::new("abc", AdminRequest::GetConfig)
            .to_json()
            .unwrap();

        let response_json = handle_request_json(&controller, &denied_operator(), &request);
        let response = ResponseEnvelope::from_json(&response_json).unwrap();

        assert_eq!(response.request_id, "abc");
        assert!(matches!(
            response.response,
            AdminResponse::Unauthorized { .. }
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn xpc_dictionary_wrapper_stores_wire_fields() {
        let dict = dictionary::OwnedObject::empty_dictionary().unwrap();
        dict.set_string(KEY_REQUEST_ID, "abc").unwrap();
        dict.set_string(KEY_PAYLOAD_JSON, "{\"ok\":true}").unwrap();
        dict.set_bool(KEY_OK, true).unwrap();

        assert!(!dict.raw().is_null());
        assert_eq!(
            dict.get_string(KEY_REQUEST_ID).unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(
            dict.get_string(KEY_PAYLOAD_JSON).unwrap().as_deref(),
            Some("{\"ok\":true}")
        );
        assert!(dict.get_bool(KEY_OK).unwrap());
    }
}
