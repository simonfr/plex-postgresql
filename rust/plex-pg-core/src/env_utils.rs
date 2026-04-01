use std::ffi::CStr;
use std::os::raw::c_char;

/// Centralized check for the PLEX_PG_TRACE_LOADONE diagnostic flag.
pub fn loadone_trace_enabled() -> bool {
    env_truthy(b"PLEX_PG_TRACE_LOADONE\0")
}

pub fn env_truthy(name: &[u8]) -> bool {
    unsafe {
        let val = libc::getenv(name.as_ptr() as *const c_char);
        if val.is_null() || *val == 0 {
            return false;
        }
        matches!(*val as u8, b'1' | b'y' | b'Y' | b't' | b'T')
    }
}

pub fn env_usize(name: &[u8]) -> Option<usize> {
    unsafe {
        let val = libc::getenv(name.as_ptr() as *const c_char);
        if val.is_null() || *val == 0 {
            return None;
        }
        let s = CStr::from_ptr(val).to_string_lossy();
        s.trim().parse::<usize>().ok()
    }
}

pub fn env_truthy_str(name: &str) -> bool {
    let Ok(raw) = std::env::var(name) else {
        return false;
    };
    let mut bytes = raw.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    matches!(first, b'1' | b'y' | b'Y' | b't' | b'T')
}

pub fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

pub fn env_string_or_else<F>(name: &str, fallback: F) -> String
where
    F: FnOnce() -> String,
{
    std::env::var(name).unwrap_or_else(|_| fallback())
}
