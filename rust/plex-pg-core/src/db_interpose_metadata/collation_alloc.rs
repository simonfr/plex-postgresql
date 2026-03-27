use super::*;
use crate::log_debug_lazy;

pub(super) fn create_collation_impl(
    db: *mut sqlite3,
    name: *const c_char,
    text_rep: c_int,
    arg: *mut c_void,
    compare: CollationCompare,
) -> c_int {
    if !name.is_null() {
        let name_bytes = unsafe { CStr::from_ptr(name).to_bytes() };
        if contains_icase_bytes(name_bytes, b"icu") {
            log_debug_lazy!(
                "Faking registration of collation: {}",
                cstr_to_string_or(name, "")
            );
            return SQLITE_OK;
        }
    }
    unsafe {
        match orig_sqlite3_create_collation {
            Some(f) => f(db, name, text_rep, arg, compare),
            None => SQLITE_ERROR,
        }
    }
}

pub(super) fn create_collation_v2_impl(
    db: *mut sqlite3,
    name: *const c_char,
    text_rep: c_int,
    arg: *mut c_void,
    compare: CollationCompare,
    destroy: CollationDestroy,
) -> c_int {
    if !name.is_null() {
        let name_bytes = unsafe { CStr::from_ptr(name).to_bytes() };
        if contains_icase_bytes(name_bytes, b"icu") {
            log_debug_lazy!(
                "Faking registration of collation v2: {}",
                cstr_to_string_or(name, "")
            );
            return SQLITE_OK;
        }
    }
    unsafe {
        match orig_sqlite3_create_collation_v2 {
            Some(f) => f(db, name, text_rep, arg, compare, destroy),
            None => SQLITE_ERROR,
        }
    }
}

pub(super) fn free_impl(ptr: *mut c_void) {
    unsafe {
        if let Some(f) = orig_sqlite3_free {
            f(ptr);
        } else {
            libc::free(ptr);
        }
    }
}

pub(super) fn malloc_impl(n: c_int) -> *mut c_void {
    unsafe {
        if let Some(f) = orig_sqlite3_malloc {
            return f(n);
        }
    }
    unsafe { libc::malloc(n as usize) }
}
