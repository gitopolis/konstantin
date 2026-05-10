//! Console-user detection via SystemConfiguration.
//!
//! Calls `SCDynamicStoreCopyConsoleUser` to ask the system who
//! currently owns the foreground Aqua console. Returns at most one
//! username — the user actually at the keyboard right now.
//!
//! Replaces an earlier `utmpx` walk that was vulnerable to two
//! issues:
//!   * `utmpx` records survive an abnormal session end (and even a
//!     graceful logout doesn't always update the record promptly), so
//!     a user who logged out would sometimes still appear "logged in"
//!     for several minutes, inflating their counter against
//!     wall-clock.
//!   * `utmpx` reports every logged-in user, including ones
//!     backgrounded by Fast User Switching. For our use case ("only
//!     one user is at the Mac at a time") only the foreground user
//!     should accrue time.
//!
//! `SCDynamicStoreCopyConsoleUser` flips on graceful logout
//! immediately and reports only the FUS-foreground user, so it is the
//! correct source of truth for both invariants.

use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr};

#[link(name = "SystemConfiguration", kind = "framework")]
#[link(name = "CoreFoundation")]
extern "C" {
    /// Returns a retained `CFStringRef` naming the current console
    /// user, or NULL if nobody is at the console. `store` may be NULL
    /// (an ephemeral store is used). `uid`/`gid` may be NULL.
    fn SCDynamicStoreCopyConsoleUser(
        store: *const c_void,
        uid: *mut u32,
        gid: *mut u32,
    ) -> *const c_void;

    fn CFStringGetLength(s: *const c_void) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(s: *const c_void, buf: *mut c_char, size: isize, encoding: u32) -> bool;
    fn CFRelease(cf: *const c_void);
}

/// `kCFStringEncodingUTF8` from `CFString.h`.
const CFSTRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// Return the set of usernames currently active on the console.
/// Always 0 or 1 entries: macOS has a single foreground console user
/// at any moment. Empty at the login window or when no user is signed
/// in.
pub fn console_users() -> HashSet<String> {
    let mut users = HashSet::new();
    let Some(name) = copy_console_user_name() else {
        return users;
    };
    // Older macOS (pre-Big Sur) returns the literal string
    // `"loginwindow"` (uid 0) at the login window instead of NULL.
    // Treat both as "no user".
    if name.is_empty() || name == "loginwindow" {
        return users;
    }
    users.insert(name);
    users
}

/// Thin safe wrapper around `SCDynamicStoreCopyConsoleUser`. Returns
/// the owned UTF-8 username, or `None` if no user is at the console
/// (or, very unexpectedly, the CFString failed to transcode).
fn copy_console_user_name() -> Option<String> {
    // SAFETY: the `Copy*` API contract is that we receive a retained
    // CFStringRef which we own and must release. `store` accepts NULL
    // (per Apple docs: "If NULL, the function uses a session-local
    // store"). uid/gid out-params accept NULL too — we read the name
    // out of the CFString and don't need them.
    let cf = unsafe {
        SCDynamicStoreCopyConsoleUser(std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    if cf.is_null() {
        return None;
    }
    // SAFETY: `cf` is non-null and points to a CFStringRef returned
    // by the SC API. We use it only for the duration of this call and
    // release it immediately after.
    let s = unsafe { cfstring_to_owned(cf) };
    // SAFETY: the CFRelease balances the implicit retain in the
    // `Copy*` API. After this line the pointer must not be reused.
    unsafe { CFRelease(cf) };
    s
}

/// Transcode a CFString to an owned UTF-8 `String`. Returns `None`
/// only if `CFStringGetCString` rejects the buffer — which should
/// never happen for ASCII usernames.
///
/// SAFETY: caller must ensure `cf` is a valid retained CFStringRef.
unsafe fn cfstring_to_owned(cf: *const c_void) -> Option<String> {
    let length = CFStringGetLength(cf);
    if length == 0 {
        return Some(String::new());
    }
    let max = CFStringGetMaximumSizeForEncoding(length, CFSTRING_ENCODING_UTF8);
    if max <= 0 {
        return None;
    }
    let cap = (max + 1) as usize;
    let mut buf: Vec<c_char> = vec![0; cap];
    if !CFStringGetCString(cf, buf.as_mut_ptr(), cap as isize, CFSTRING_ENCODING_UTF8) {
        return None;
    }
    CStr::from_ptr(buf.as_ptr())
        .to_str()
        .ok()
        .map(|s| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: enumeration must not panic and must return at most
    /// one entry. We can't assert *who* is logged in (depends on the
    /// host) — only that the call returns a sensibly-sized set.
    #[test]
    fn enumerate_returns_at_most_one() {
        let users = console_users();
        assert!(
            users.len() <= 1,
            "expected 0 or 1 console users, got {}: {:?}",
            users.len(),
            users
        );
    }
}
