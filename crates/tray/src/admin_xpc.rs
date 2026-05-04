//! Tray-side admin XPC client.
//!
//! This module is intentionally synchronous. Menu actions should call it from
//! the existing progress/background worker path, not directly on the AppKit
//! main thread.

use anyhow::{Context, Result};
use block2::{Block, RcBlock};
use konstantin_proto::admin::{
    AdminRequest, AdminResponse, RequestEnvelope, ResponseEnvelope, KEY_PAYLOAD_JSON,
    MACH_SERVICE_NAME,
};
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

type XpcObject = *mut c_void;
type XpcConnection = XpcObject;
type DispatchQueue = *mut c_void;
type XpcHandlerBlock = Block<dyn Fn(XpcObject)>;

extern "C" {
    fn xpc_connection_create_mach_service(
        name: *const c_char,
        targetq: DispatchQueue,
        flags: u64,
    ) -> XpcConnection;
    fn xpc_connection_set_peer_team_identity_requirement(
        connection: XpcConnection,
        signing_identifier: *const c_char,
    ) -> i32;
    fn xpc_connection_set_event_handler(
        connection: XpcConnection,
        handler: *mut XpcHandlerBlock,
    );
    fn xpc_connection_activate(connection: XpcConnection);
    fn xpc_connection_cancel(connection: XpcConnection);
    fn xpc_connection_send_message_with_reply_sync(
        connection: XpcConnection,
        message: XpcObject,
    ) -> XpcObject;

    fn xpc_dictionary_create_empty() -> XpcObject;
    fn xpc_dictionary_set_string(xdict: XpcObject, key: *const c_char, value: *const c_char);
    fn xpc_dictionary_get_string(xdict: XpcObject, key: *const c_char) -> *const c_char;

    fn xpc_release(object: XpcObject);
}

pub struct AdminClient;

impl AdminClient {
    pub fn send(request: AdminRequest) -> Result<AdminResponse> {
        let connection = Connection::connect()?;
        let request_id = request_id();
        let envelope = RequestEnvelope::new(&request_id, request);
        let request_json = envelope.to_json()?;
        let message = OwnedObject::empty_dictionary()?;
        message.set_string(KEY_PAYLOAD_JSON, &request_json)?;

        let reply_raw =
            unsafe { xpc_connection_send_message_with_reply_sync(connection.raw(), message.raw()) };
        let reply = OwnedObject::from_raw(reply_raw)
            .context("admin XPC request did not receive a reply")?;
        let response_json = reply
            .get_string(KEY_PAYLOAD_JSON)?
            .ok_or_else(|| anyhow::anyhow!("admin XPC reply missing payload_json"))?;
        let response = ResponseEnvelope::from_json(&response_json)?;
        if response.request_id != request_id {
            anyhow::bail!(
                "admin XPC reply id mismatch: got {}, expected {}",
                response.request_id,
                request_id
            );
        }
        Ok(response.response)
    }
}

struct Connection {
    raw: XpcConnection,
    _handler: RcBlock<dyn Fn(XpcObject)>,
}

impl Connection {
    fn connect() -> Result<Self> {
        let service = CString::new(MACH_SERVICE_NAME).context("admin XPC service name")?;
        let raw =
            unsafe { xpc_connection_create_mach_service(service.as_ptr(), ptr::null_mut(), 0) };
        if raw.is_null() {
            anyhow::bail!("xpc_connection_create_mach_service returned NULL");
        }

        let peer_req_status = unsafe {
            // NULL signing identifier means: require the peer to be signed by
            // the same Team ID as this tray. The daemon binary's exact nested
            // code-signing identifier can vary until the SMAppService bundle
            // migration settles, so pinning it here would be premature.
            xpc_connection_set_peer_team_identity_requirement(raw, ptr::null())
        };
        if peer_req_status != 0 {
            unsafe {
                xpc_release(raw);
            }
            anyhow::bail!("setting admin XPC peer Team ID requirement failed: {peer_req_status}");
        }

        let handler = RcBlock::new(|event: XpcObject| {
            let _ = event;
        });

        unsafe {
            xpc_connection_set_event_handler(raw, RcBlock::as_ptr(&handler));
            xpc_connection_activate(raw);
        }
        Ok(Self {
            raw,
            _handler: handler,
        })
    }

    fn raw(&self) -> XpcConnection {
        self.raw
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                xpc_connection_cancel(self.raw);
                xpc_release(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

struct OwnedObject {
    raw: XpcObject,
}

impl OwnedObject {
    fn from_raw(raw: XpcObject) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self { raw })
        }
    }

    fn empty_dictionary() -> Result<Self> {
        let raw = unsafe { xpc_dictionary_create_empty() };
        Self::from_raw(raw)
            .ok_or_else(|| anyhow::anyhow!("xpc_dictionary_create_empty returned NULL"))
    }

    fn raw(&self) -> XpcObject {
        self.raw
    }

    fn set_string(&self, key: &str, value: &str) -> Result<()> {
        let key = cstring(key, "dictionary key")?;
        let value = cstring(value, "dictionary string value")?;
        unsafe {
            xpc_dictionary_set_string(self.raw, key.as_ptr(), value.as_ptr());
        }
        Ok(())
    }

    fn get_string(&self, key: &str) -> Result<Option<String>> {
        let key = cstring(key, "dictionary key")?;
        let ptr = unsafe { xpc_dictionary_get_string(self.raw, key.as_ptr()) };
        if ptr.is_null() {
            return Ok(None);
        }
        Ok(Some(
            unsafe { CStr::from_ptr(ptr) }
                .to_str()
                .context("XPC string was not UTF-8")?
                .to_string(),
        ))
    }
}

impl Drop for OwnedObject {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                xpc_release(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

fn cstring(value: &str, what: &str) -> Result<CString> {
    CString::new(value).with_context(|| format!("{what} contains an interior NUL byte"))
}

fn request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_round_trips_payload() {
        let dict = OwnedObject::empty_dictionary().unwrap();
        dict.set_string(KEY_PAYLOAD_JSON, "{\"hello\":\"world\"}")
            .unwrap();

        assert_eq!(
            dict.get_string(KEY_PAYLOAD_JSON).unwrap().as_deref(),
            Some("{\"hello\":\"world\"}")
        );
    }

    #[test]
    fn generated_request_ids_are_nonempty() {
        assert!(!request_id().is_empty());
    }
}
