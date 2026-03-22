use std::ffi::CStr;
use std::os::raw::c_char;

pub fn contains_ascii_icase(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

pub fn starts_with_ascii_icase(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

pub fn contains_icase_ptr(ptr: *const c_char, needle: &str) -> bool {
    if ptr.is_null() {
        return false;
    }
    let hay = unsafe { CStr::from_ptr(ptr).to_bytes() };
    contains_ascii_icase(hay, needle.as_bytes())
}
