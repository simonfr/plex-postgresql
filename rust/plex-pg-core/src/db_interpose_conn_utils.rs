use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

use crate::ffi_types::PgConnection;
use crate::libpq_helpers::{PGcancel, PGconn, PGresult};

pub(crate) const STATEMENT_TIMEOUT_SQL: &str = "SET statement_timeout = '5min'";

#[repr(C)]
pub struct PgConnConfig {
    pub host: [c_char; 256],
    pub port: c_int,
    pub database: [c_char; 128],
    pub user: [c_char; 128],
    pub password: [c_char; 256],
    pub schema: [c_char; 64],
}

pub struct PthreadMutexGuard {
    mutex: *mut libc::pthread_mutex_t,
    locked: bool,
}

impl PthreadMutexGuard {
    pub unsafe fn lock(mutex: *mut libc::pthread_mutex_t) -> Self {
        libc::pthread_mutex_lock(mutex);
        Self {
            mutex,
            locked: true,
        }
    }

    pub unsafe fn adopt(mutex: *mut libc::pthread_mutex_t) -> Self {
        Self {
            mutex,
            locked: true,
        }
    }

    pub unsafe fn unlock(&mut self) {
        if self.locked {
            libc::pthread_mutex_unlock(self.mutex);
            self.locked = false;
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn mutex_ptr(&self) -> *mut libc::pthread_mutex_t {
        self.mutex
    }
}

impl Drop for PthreadMutexGuard {
    fn drop(&mut self) {
        if self.locked {
            unsafe {
                libc::pthread_mutex_unlock(self.mutex);
            }
        }
    }
}

pub(crate) fn log_error(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(0, cs.as_ptr());
    }
}

pub(crate) fn log_info(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(1, cs.as_ptr());
    }
}

pub(crate) fn log_debug(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(2, cs.as_ptr());
    }
}

pub(crate) fn cstr_to_str(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "STEP";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("STEP") }
}

pub(crate) fn cstr_to_string_or(ptr: *const c_char, default: &str) -> String {
    if ptr.is_null() {
        return default.to_string();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

pub(crate) fn cstr_prefix(ptr: *const c_char, max_len: usize, default: &str) -> String {
    if ptr.is_null() {
        return default.to_string();
    }
    let bytes = unsafe { CStr::from_ptr(ptr).to_bytes() };
    let slice = &bytes[..bytes.len().min(max_len)];
    String::from_utf8_lossy(slice).into_owned()
}

pub(crate) fn cbuf_to_str(buf: &[c_char]) -> &str {
    if buf.is_empty() {
        return "";
    }
    unsafe { CStr::from_ptr(buf.as_ptr()).to_str().unwrap_or("") }
}

pub(crate) fn build_conninfo(cfg: &PgConnConfig) -> CString {
    let conninfo = format!(
        "host={} port={} dbname={} user={} password={} connect_timeout=5 keepalives=1 keepalives_idle=30 keepalives_interval=10 keepalives_count=3",
        cbuf_to_str(&cfg.host),
        cfg.port,
        cbuf_to_str(&cfg.database),
        cbuf_to_str(&cfg.user),
        cbuf_to_str(&cfg.password)
    );
    let safe = conninfo.replace('\0', "");
    CString::new(safe).unwrap_or_else(|_| CString::new(" ").unwrap())
}

pub(crate) unsafe fn connect_new(cfg: &PgConnConfig) -> *mut PGconn {
    let conninfo = build_conninfo(cfg);
    crate::libpq_helpers::rust_pq_connectdb(conninfo.as_ptr())
}

pub(crate) unsafe fn apply_pg_session_settings(conn: *mut PGconn, cfg: &PgConnConfig) {
    if conn.is_null() {
        return;
    }
    let schema_cmd = format!("SET search_path TO {}, public", cbuf_to_str(&cfg.schema));
    if let Ok(schema_c) = CString::new(schema_cmd) {
        let res = crate::libpq_helpers::rust_pq_exec(conn, schema_c.as_ptr());
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
    }
    if let Ok(timeout_c) = CString::new(STATEMENT_TIMEOUT_SQL) {
        let res = crate::libpq_helpers::rust_pq_exec(conn, timeout_c.as_ptr());
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
    }
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
            let mut errbuf = [0 as c_char; 256];
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
