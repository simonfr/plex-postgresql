use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

use crate::ffi_types::PgConnection;
use crate::libpq_helpers::{PGcancel, PGresult};

fn log_info(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(1, cs.as_ptr());
    }
}

fn cstr_to_str(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "STEP";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("STEP") }
}

#[no_mangle]
pub extern "C" fn rust_step_conn_cancel_and_drain(
    conn: *mut PgConnection,
    scope_tag: *const c_char,
) {
    if conn.is_null() {
        return;
    }
    unsafe {
        if (*conn).conn.is_null() {
            return;
        }
        if (*conn).streaming_active.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            return;
        }

        crate::libpq_helpers::rust_pq_set_nonblocking((*conn).conn, 0);
        while crate::libpq_helpers::rust_pq_is_busy((*conn).conn) != 0 {
            crate::libpq_helpers::rust_pq_consume_input((*conn).conn);
        }

        let cancel: *mut PGcancel = crate::libpq_helpers::rust_pq_get_cancel((*conn).conn);
        if !cancel.is_null() {
            let mut errbuf = [0i8; 256];
            crate::libpq_helpers::rust_pq_cancel(
                cancel,
                errbuf.as_mut_ptr(),
                errbuf.len() as c_int,
            );
            crate::libpq_helpers::rust_pq_free_cancel(cancel);
        }

        let mut drain_count = 0;
        loop {
            let pending: *mut PGresult = crate::libpq_helpers::rust_pq_get_result((*conn).conn);
            if pending.is_null() {
                break;
            }
            drain_count += 1;
            if drain_count <= 3 {
                let status = crate::libpq_helpers::rust_pq_result_status(pending);
                let status_str = cstr_to_str(crate::libpq_helpers::rust_pq_res_status(status));
                let tag = cstr_to_str(scope_tag);
                log_info(&format!(
                    "{}: Drained orphaned result from connection {:p} (status={}: {})",
                    tag, conn, status, status_str
                ));
            }
            crate::libpq_helpers::rust_pq_clear(pending);
            if drain_count > 1000 {
                let tag = cstr_to_str(scope_tag);
                log_info(&format!(
                    "{}: Drain loop exceeded 1000 on {:p} - aborting drain",
                    tag, conn
                ));
                break;
            }
        }
        if drain_count > 3 {
            let tag = cstr_to_str(scope_tag);
            log_info(&format!(
                "{}: Drained {} orphaned results total from connection {:p}",
                tag, drain_count, conn
            ));
        }
    }
}
