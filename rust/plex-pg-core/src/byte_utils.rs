use std::ffi::CStr;
use std::os::raw::c_char;

pub(crate) fn ascii_lower(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

pub(crate) fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

pub(crate) fn contains_icase_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.iter().zip(needle.iter()).all(|(a, b)| ascii_lower(*a) == ascii_lower(*b)))
}

pub(crate) fn starts_with_icase_bytes(haystack: &[u8], prefix: &[u8]) -> bool {
    if prefix.is_empty() || haystack.len() < prefix.len() {
        return false;
    }
    haystack[..prefix.len()]
        .iter()
        .zip(prefix.iter())
        .all(|(a, b)| ascii_lower(*a) == ascii_lower(*b))
}

pub(crate) unsafe fn cstr_bytes<'a>(ptr: *const c_char) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    CStr::from_ptr(ptr).to_bytes()
}
