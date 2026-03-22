use std::cell::UnsafeCell;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

pub(crate) fn env_truthy(name: &[u8]) -> bool {
    unsafe {
        let val = libc::getenv(name.as_ptr() as *const c_char);
        if val.is_null() || *val == 0 {
            return false;
        }
        matches!(*val as u8, b'1' | b'y' | b'Y' | b't' | b'T')
    }
}

pub(crate) fn log_info(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(1, cs.as_ptr());
    }
}

pub(crate) fn log_shim_loaded(os_label: &str) {
    log_info(&format!(
        "=== Plex PostgreSQL Interpose Shim loaded ({}) ===",
        os_label
    ));
}

pub(crate) fn log_shim_unloading(os_label: &str) {
    log_info(&format!(
        "=== Plex PostgreSQL Interpose Shim unloading ({}) ===",
        os_label
    ));
}

thread_local! {
    static IN_EXCEPTION_HANDLER: UnsafeCell<c_int> = UnsafeCell::new(0);
}

pub(crate) fn handle_exception_with_tls(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
) -> (c_int, c_int) {
    let mut should_call_original: c_int = 1;
    let handled = IN_EXCEPTION_HANDLER.with(|cell| {
        let guard = cell.get();
        crate::db_interpose_common::common_handle_exception(
            thrown_exception,
            tinfo,
            guard,
            &mut should_call_original,
        )
    });
    (handled, should_call_original)
}
