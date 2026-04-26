//! Console-session enumeration via the macOS `utmpx` database.
//!
//! Walks `/var/run/utmpx` through libc's `setutxent` / `getutxent` /
//! `endutxent` and returns the set of usernames that currently hold a
//! `USER_PROCESS` record on a tty whose `ut_line` starts with `console`
//! (the loginwindow / Aqua console). SSH (`ttys000`, …) and other ptys
//! are filtered out — per the design in CLAUDE.md, only console time
//! counts.
//!
//! Note: the libc utmpx cursor is **process-wide global state**. Only one
//! task may walk it at a time. The daemon's tick task is the only caller.

use std::collections::HashSet;

/// `ut_type` value for a live login session. See `<utmpx.h>`.
const USER_PROCESS: libc::c_short = 7;

/// Return the set of usernames currently logged in on a console session.
pub fn console_users() -> HashSet<String> {
    let mut users = HashSet::new();
    // SAFETY: `setutxent` / `getutxent` / `endutxent` form the documented
    // walk protocol. We never retain the returned pointer past the next
    // call (we copy the username into an owned `String` immediately).
    unsafe {
        libc::setutxent();
        loop {
            let raw = libc::getutxent();
            if raw.is_null() {
                break;
            }
            let entry = &*raw;
            if entry.ut_type != USER_PROCESS {
                continue;
            }
            if !c_field_starts_with(&entry.ut_line, b"console") {
                continue;
            }
            let user = c_field_to_string(&entry.ut_user);
            if !user.is_empty() {
                users.insert(user);
            }
        }
        libc::endutxent();
    }
    users
}

/// Read a fixed-width C `char` array (as found in `utmpx`) up to its first
/// NUL byte and return an owned, lossy-UTF-8 `String`.
fn c_field_to_string(field: &[libc::c_char]) -> String {
    let bytes = c_field_bytes(field);
    String::from_utf8_lossy(bytes).into_owned()
}

fn c_field_starts_with(field: &[libc::c_char], prefix: &[u8]) -> bool {
    c_field_bytes(field).starts_with(prefix)
}

fn c_field_bytes(field: &[libc::c_char]) -> &[u8] {
    // SAFETY: `c_char` and `u8` have the same size and alignment; we only
    // read the bytes, we don't reinterpret them as anything signed.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(field.as_ptr() as *const u8, field.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_field_handles_nul_termination() {
        let mut buf = [0i8; 8];
        for (i, b) in b"hi\0xxx".iter().enumerate() {
            buf[i] = *b as i8;
        }
        assert_eq!(c_field_to_string(&buf), "hi");
        assert!(c_field_starts_with(&buf, b"hi"));
        assert!(!c_field_starts_with(&buf, b"hello"));
    }

    #[test]
    fn c_field_handles_no_nul() {
        let mut buf = [0i8; 4];
        for (i, b) in b"abcd".iter().enumerate() {
            buf[i] = *b as i8;
        }
        assert_eq!(c_field_to_string(&buf), "abcd");
    }

    /// Smoke test: enumeration must not panic. We can't assert who's logged
    /// in (depends on the host), only that the call returns.
    #[test]
    fn enumerate_does_not_panic() {
        let _ = console_users();
    }
}
