use std::ffi::CString;
use std::os::raw::c_char;

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
