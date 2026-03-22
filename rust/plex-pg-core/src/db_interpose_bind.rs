use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::db_interpose_conn_utils::{cstr_to_string_or, log_debug, PthreadMutexGuard};
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgStmt, MAX_PARAMS, PARAM_BUF_LEN};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_MISUSE: c_int = 21;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;

const PMT_BIND_TEXT_ALLOC: c_int = 0;
const PMT_BIND_HEX_ALLOC: c_int = 1;
const PMT_BIND_VALUE_BLOB_ALLOC: c_int = 2;

static BIND_RESET_DISABLED: AtomicI32 = AtomicI32::new(-1);

static PHASE_BIND_INT: &[u8] = b"bind_int\0";
static PHASE_BIND_INT64: &[u8] = b"bind_int64\0";
static PHASE_BIND_DOUBLE: &[u8] = b"bind_double\0";
static PHASE_BIND_TEXT: &[u8] = b"bind_text\0";
static PHASE_BIND_TEXT64: &[u8] = b"bind_text64\0";
static PHASE_BIND_BLOB: &[u8] = b"bind_blob\0";
static PHASE_BIND_BLOB64: &[u8] = b"bind_blob64\0";
static PHASE_BIND_VALUE: &[u8] = b"bind_value\0";
static PHASE_BIND_NULL: &[u8] = b"bind_null\0";

extern "C" {
    static mut orig_sqlite3_bind_int: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, c_int) -> c_int>;
    static mut orig_sqlite3_bind_int64: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, i64) -> c_int>;
    static mut orig_sqlite3_bind_double: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, f64) -> c_int>;
    static mut orig_sqlite3_bind_text: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_char, c_int, *mut c_void) -> c_int,
    >;
    static mut orig_sqlite3_bind_text64: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_char, u64, *mut c_void, c_uchar) -> c_int,
    >;
    static mut orig_sqlite3_bind_blob: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_void, c_int, *mut c_void) -> c_int,
    >;
    static mut orig_sqlite3_bind_blob64: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_void, u64, *mut c_void) -> c_int,
    >;
    static mut orig_sqlite3_bind_value: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const sqlite3_value) -> c_int>;
    static mut orig_sqlite3_bind_null: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int>;
    static mut orig_sqlite3_reset: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char>;
    static mut orig_sqlite3_db_handle: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut sqlite3>;

    fn pg_find_any_stmt(stmt: *mut sqlite3_stmt) -> *mut PgStmt;
    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
}

unsafe fn pg_map_param_index(pg_stmt: *mut PgStmt, p_stmt: *mut sqlite3_stmt, sqlite_idx: c_int) -> c_int {
    if pg_stmt.is_null() {
        log_debug(&format!(
            "pg_map_param_index: no pg_stmt, using direct mapping idx={} -> {}",
            sqlite_idx,
            sqlite_idx - 1
        ));
        return sqlite_idx - 1;
    }

    if !(*pg_stmt).param_names.is_null() && (*pg_stmt).param_count > 0 {
        let param_name = crate::db_interpose_metadata::rust_my_sqlite3_bind_parameter_name(p_stmt, sqlite_idx);
        log_debug(&format!(
            "pg_map_param_index: sqlite_idx={}, param_name={}, param_count={}",
            sqlite_idx,
            cstr_to_string_or(param_name, "NULL"),
            (*pg_stmt).param_count
        ));

        if !param_name.is_null() {
            let mut clean_name = param_name;
            if *param_name == b':' as c_char {
                clean_name = param_name.add(1);
            }

            let param_count = (*pg_stmt).param_count as usize;
            let max_debug = param_count.min(5);
            for i in 0..max_debug {
                let cur = *(*pg_stmt).param_names.add(i);
                log_debug(&format!(
                    "  param_names[{}] = {}",
                    i,
                    cstr_to_string_or(cur, "NULL")
                ));
            }

            for i in 0..param_count {
                let cur = *(*pg_stmt).param_names.add(i);
                if !cur.is_null() && libc::strcmp(cur, clean_name) == 0 {
                    log_debug(&format!("  -> Found match at pg_idx={}", i));
                    return i as c_int;
                }
            }
            log_debug(&format!(
                "Named parameter '{}' not found in translation (sqlite_idx={})",
                cstr_to_string_or(clean_name, "NULL"),
                sqlite_idx
            ));
        } else {
            log_debug("  -> No parameter name, using direct mapping");
        }
    } else {
        log_debug(&format!(
            "pg_map_param_index: no param_names (count={}), using direct mapping idx={} -> {}",
            (*pg_stmt).param_count,
            sqlite_idx,
            sqlite_idx - 1
        ));
    }

    sqlite_idx - 1
}

fn note_bind_phase(phase: &[u8], p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) {
    let mut sql: *const c_char = ptr::null();
    let mut db: *mut sqlite3 = ptr::null_mut();

    if !pg_stmt.is_null() {
        unsafe {
            sql = if !(*pg_stmt).pg_sql.is_null() {
                (*pg_stmt).pg_sql
            } else {
                (*pg_stmt).sql
            };
        }
    }

    if sql.is_null() {
        unsafe {
            if let Some(f) = orig_sqlite3_sql {
                sql = f(p_stmt);
            }
        }
    }

    unsafe {
        if let Some(f) = orig_sqlite3_db_handle {
            db = f(p_stmt);
        }
    }

    unsafe {
        pg_exception_note_phase(phase.as_ptr() as *const c_char, sql, p_stmt, db);
    }
}

fn bind_reset_disabled() -> bool {
    let cached = BIND_RESET_DISABLED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let name = b"PLEX_PG_DISABLE_BIND_RESET\0";
    let val = unsafe {
        let env = libc::getenv(name.as_ptr() as *const c_char);
        crate::db_interpose_helpers::rust_env_truthy(env)
    };
    let flag = if val != 0 { 1 } else { 0 };
    BIND_RESET_DISABLED.store(flag, Ordering::Relaxed);
    flag == 1
}

fn contains_binary_bytes(data: *const u8, len: usize) -> bool {
    crate::db_interpose_helpers::rust_contains_binary_bytes(data, len) != 0
}

unsafe fn bytes_to_pg_hex(data: *const u8, len: usize) -> *mut c_char {
    let hex_rust = crate::db_interpose_helpers::rust_bytes_to_pg_hex(data, len);
    if hex_rust.is_null() {
        return ptr::null_mut();
    }
    let hex = libc::strdup(hex_rust);
    crate::db_interpose_helpers::rust_free_cstring(hex_rust);
    if hex.is_null() {
        return ptr::null_mut();
    }
    if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
        let bytes = libc::strlen(hex) as u64 + 1;
        crate::pg_mem_telemetry::rust_mem_telemetry_add(PMT_BIND_HEX_ALLOC, bytes, 1);
    }
    hex
}

fn should_reset_stmt(pg_stmt: *mut PgStmt) -> bool {
    if bind_reset_disabled() {
        return false;
    }
    if pg_stmt.is_null() {
        return false;
    }
    unsafe { (*pg_stmt).is_pg != 0 }
}

unsafe fn wait_for_stmt_ready(p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) -> bool {
    if should_reset_stmt(pg_stmt) {
        if let Some(f) = orig_sqlite3_reset {
            f(p_stmt);
        }
    }
    libc::usleep(500);
    true
}

unsafe fn ensure_stmt_not_busy(p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) {
    if should_reset_stmt(pg_stmt) {
        if let Some(f) = orig_sqlite3_reset {
            f(p_stmt);
        }
    }
}

unsafe fn clear_metadata_result_if_needed(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }
    if (*pg_stmt).metadata_only_result != 0 && !(*pg_stmt).result.is_null() {
        log_debug("BIND: Marking metadata-only result for re-execution with bound params");
        (*pg_stmt).metadata_only_result = 2;
    }
}

unsafe fn is_preallocated_buffer(stmt: *mut PgStmt, idx: usize) -> bool {
    if stmt.is_null() || idx >= MAX_PARAMS {
        return false;
    }
    let val = (*stmt).param_values[idx];
    if val.is_null() {
        return false;
    }
    let val_addr = val as usize;
    let base = (*stmt).param_buffers[idx].as_ptr() as usize;
    val_addr >= base && val_addr < base + PARAM_BUF_LEN
}

unsafe fn retry_on_misuse<F>(
    mut rc: c_int,
    p_stmt: *mut sqlite3_stmt,
    pg_stmt: *mut PgStmt,
    mut bind_call: F,
) -> c_int
where
    F: FnMut() -> c_int,
{
    if rc != SQLITE_MISUSE {
        return rc;
    }
    for _ in 0..3 {
        if wait_for_stmt_ready(p_stmt, pg_stmt) {
            rc = bind_call();
            if rc == SQLITE_OK {
                break;
            }
        }
        if rc != SQLITE_MISUSE {
            break;
        }
    }
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_int(p_stmt: *mut sqlite3_stmt, idx: c_int, val: c_int) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_INT, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc = unsafe { orig_sqlite3_bind_int.map(|f| f(p_stmt, idx, val)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_int.map(|f| f(p_stmt, idx, val)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                libc::snprintf(
                    (*pg_stmt).param_buffers[pg_idx as usize].as_mut_ptr(),
                    PARAM_BUF_LEN,
                    b"%d\0".as_ptr() as *const c_char,
                    val,
                );
                (*pg_stmt).param_values[pg_idx as usize] =
                    (*pg_stmt).param_buffers[pg_idx as usize].as_mut_ptr();
            }
        }
    }

    drop(guard);
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_int64(p_stmt: *mut sqlite3_stmt, idx: c_int, val: i64) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_INT64, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc = unsafe { orig_sqlite3_bind_int64.map(|f| f(p_stmt, idx, val)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_int64.map(|f| f(p_stmt, idx, val)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                libc::snprintf(
                    (*pg_stmt).param_buffers[pg_idx as usize].as_mut_ptr(),
                    PARAM_BUF_LEN,
                    b"%lld\0".as_ptr() as *const c_char,
                    val as libc::c_longlong,
                );
                (*pg_stmt).param_values[pg_idx as usize] =
                    (*pg_stmt).param_buffers[pg_idx as usize].as_mut_ptr();
            }
        }
    }

    drop(guard);
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_double(p_stmt: *mut sqlite3_stmt, idx: c_int, val: f64) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_DOUBLE, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc = unsafe { orig_sqlite3_bind_double.map(|f| f(p_stmt, idx, val)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_double.map(|f| f(p_stmt, idx, val)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                libc::snprintf(
                    (*pg_stmt).param_buffers[pg_idx as usize].as_mut_ptr(),
                    PARAM_BUF_LEN,
                    b"%.17g\0".as_ptr() as *const c_char,
                    val,
                );
                (*pg_stmt).param_values[pg_idx as usize] =
                    (*pg_stmt).param_buffers[pg_idx as usize].as_mut_ptr();
            }
        }
    }

    drop(guard);
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_text(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_TEXT, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc =
        unsafe { orig_sqlite3_bind_text.map(|f| f(p_stmt, idx, val, n_bytes, destructor)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_text.map(|f| f(p_stmt, idx, val, n_bytes, destructor)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int && !val.is_null() {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                if !(*pg_stmt).param_values[pg_idx as usize].is_null()
                    && !is_preallocated_buffer(pg_stmt, pg_idx as usize)
                {
                    libc::free((*pg_stmt).param_values[pg_idx as usize] as *mut c_void);
                    (*pg_stmt).param_values[pg_idx as usize] = ptr::null_mut();
                }

                let actual_len = if n_bytes < 0 {
                    libc::strlen(val) as usize
                } else {
                    n_bytes as usize
                };

                if contains_binary_bytes(val as *const u8, actual_len) {
                    log_debug(&format!(
                        "bind_text: detected binary data at idx={}, len={}, converting to hex",
                        idx, actual_len
                    ));
                    (*pg_stmt).param_values[pg_idx as usize] = bytes_to_pg_hex(val as *const u8, actual_len);
                } else if n_bytes < 0 {
                    (*pg_stmt).param_values[pg_idx as usize] = libc::strdup(val);
                    if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
                        crate::pg_mem_telemetry::rust_mem_telemetry_add(
                            PMT_BIND_TEXT_ALLOC,
                            actual_len as u64 + 1,
                            1,
                        );
                    }
                } else {
                    (*pg_stmt).param_values[pg_idx as usize] =
                        libc::malloc(n_bytes as usize + 1) as *mut c_char;
                    if !(*pg_stmt).param_values[pg_idx as usize].is_null() {
                        libc::memcpy(
                            (*pg_stmt).param_values[pg_idx as usize] as *mut c_void,
                            val as *const c_void,
                            n_bytes as usize,
                        );
                        *(*pg_stmt).param_values[pg_idx as usize].add(n_bytes as usize) = 0;
                        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
                            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                                PMT_BIND_TEXT_ALLOC,
                                n_bytes as u64 + 1,
                                1,
                            );
                        }
                    }
                }
            }
        }
    }

    drop(guard);
    crate::pg_mem_telemetry::rust_mem_telemetry_maybe_log();
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_blob(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_BLOB, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc =
        unsafe { orig_sqlite3_bind_blob.map(|f| f(p_stmt, idx, val, n_bytes, destructor)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_blob.map(|f| f(p_stmt, idx, val, n_bytes, destructor)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int && !val.is_null() && n_bytes > 0 {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                if !(*pg_stmt).param_values[pg_idx as usize].is_null()
                    && !is_preallocated_buffer(pg_stmt, pg_idx as usize)
                {
                    libc::free((*pg_stmt).param_values[pg_idx as usize] as *mut c_void);
                    (*pg_stmt).param_values[pg_idx as usize] = ptr::null_mut();
                }
                log_debug(&format!(
                    "bind_blob: converting {} bytes to hex at idx={}",
                    n_bytes, idx
                ));
                (*pg_stmt).param_values[pg_idx as usize] = bytes_to_pg_hex(val as *const u8, n_bytes as usize);
                (*pg_stmt).param_lengths[pg_idx as usize] = 0;
                (*pg_stmt).param_formats[pg_idx as usize] = 0;
            }
        }
    }

    drop(guard);
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_blob64(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: u64,
    destructor: *mut c_void,
) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_BLOB64, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc =
        unsafe { orig_sqlite3_bind_blob64.map(|f| f(p_stmt, idx, val, n_bytes, destructor)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_blob64.map(|f| f(p_stmt, idx, val, n_bytes, destructor)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int && !val.is_null() && n_bytes > 0 {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                if !(*pg_stmt).param_values[pg_idx as usize].is_null()
                    && !is_preallocated_buffer(pg_stmt, pg_idx as usize)
                {
                    libc::free((*pg_stmt).param_values[pg_idx as usize] as *mut c_void);
                    (*pg_stmt).param_values[pg_idx as usize] = ptr::null_mut();
                }
                log_debug(&format!(
                    "bind_blob64: converting {} bytes to hex at idx={}",
                    n_bytes, idx
                ));
                (*pg_stmt).param_values[pg_idx as usize] = bytes_to_pg_hex(val as *const u8, n_bytes as usize);
                (*pg_stmt).param_lengths[pg_idx as usize] = 0;
                (*pg_stmt).param_formats[pg_idx as usize] = 0;
            }
        }
    }

    drop(guard);
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_text64(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n_bytes: u64,
    destructor: *mut c_void,
    encoding: c_uchar,
) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_TEXT64, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc =
        unsafe { orig_sqlite3_bind_text64.map(|f| f(p_stmt, idx, val, n_bytes, destructor, encoding)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_text64.map(|f| f(p_stmt, idx, val, n_bytes, destructor, encoding)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int && !val.is_null() {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                if !(*pg_stmt).param_values[pg_idx as usize].is_null()
                    && !is_preallocated_buffer(pg_stmt, pg_idx as usize)
                {
                    libc::free((*pg_stmt).param_values[pg_idx as usize] as *mut c_void);
                    (*pg_stmt).param_values[pg_idx as usize] = ptr::null_mut();
                }

                let actual_len = if n_bytes == u64::MAX {
                    libc::strlen(val) as usize
                } else {
                    n_bytes as usize
                };

                if contains_binary_bytes(val as *const u8, actual_len) {
                    log_debug(&format!(
                        "bind_text64: detected binary data at idx={}, len={}, converting to hex",
                        idx, actual_len
                    ));
                    (*pg_stmt).param_values[pg_idx as usize] = bytes_to_pg_hex(val as *const u8, actual_len);
                } else if n_bytes == u64::MAX {
                    (*pg_stmt).param_values[pg_idx as usize] = libc::strdup(val);
                    if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
                        crate::pg_mem_telemetry::rust_mem_telemetry_add(
                            PMT_BIND_TEXT_ALLOC,
                            actual_len as u64 + 1,
                            1,
                        );
                    }
                } else {
                    (*pg_stmt).param_values[pg_idx as usize] =
                        libc::malloc(n_bytes as usize + 1) as *mut c_char;
                    if !(*pg_stmt).param_values[pg_idx as usize].is_null() {
                        libc::memcpy(
                            (*pg_stmt).param_values[pg_idx as usize] as *mut c_void,
                            val as *const c_void,
                            n_bytes as usize,
                        );
                        *(*pg_stmt).param_values[pg_idx as usize].add(n_bytes as usize) = 0;
                        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
                            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                                PMT_BIND_TEXT_ALLOC,
                                n_bytes + 1,
                                1,
                            );
                        }
                    }
                }
            }
        }
    }

    drop(guard);
    crate::pg_mem_telemetry::rust_mem_telemetry_maybe_log();
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_value(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    p_value: *const sqlite3_value,
) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_VALUE, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc =
        unsafe { orig_sqlite3_bind_value.map(|f| f(p_stmt, idx, p_value)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_value.map(|f| f(p_stmt, idx, p_value)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int && !p_value.is_null() {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                let mutable_value = p_value as *mut sqlite3_value;
                let vtype = crate::db_interpose_value::rust_my_sqlite3_value_type(mutable_value);
                if !(*pg_stmt).param_values[pg_idx as usize].is_null()
                    && !is_preallocated_buffer(pg_stmt, pg_idx as usize)
                {
                    libc::free((*pg_stmt).param_values[pg_idx as usize] as *mut c_void);
                    (*pg_stmt).param_values[pg_idx as usize] = ptr::null_mut();
                }

                match vtype {
                    SQLITE_INTEGER => {
                        let v = crate::db_interpose_value::rust_my_sqlite3_value_int64(mutable_value);
                        let mut buf = [0 as c_char; 32];
                        libc::snprintf(
                            buf.as_mut_ptr(),
                            buf.len(),
                            b"%lld\0".as_ptr() as *const c_char,
                            v as libc::c_longlong,
                        );
                        (*pg_stmt).param_values[pg_idx as usize] = libc::strdup(buf.as_ptr());
                    }
                    SQLITE_FLOAT => {
                        let v = crate::db_interpose_value::rust_my_sqlite3_value_double(mutable_value);
                        let mut buf = [0 as c_char; 64];
                        libc::snprintf(
                            buf.as_mut_ptr(),
                            buf.len(),
                            b"%.17g\0".as_ptr() as *const c_char,
                            v,
                        );
                        (*pg_stmt).param_values[pg_idx as usize] = libc::strdup(buf.as_ptr());
                    }
                    SQLITE_TEXT => {
                        let v = crate::db_interpose_value::rust_my_sqlite3_value_text(mutable_value);
                        if !v.is_null() {
                            (*pg_stmt).param_values[pg_idx as usize] = libc::strdup(v as *const c_char);
                        }
                    }
                    SQLITE_BLOB => {
                        let len = crate::db_interpose_value::rust_my_sqlite3_value_bytes(mutable_value);
                        let v = crate::db_interpose_value::rust_my_sqlite3_value_blob(mutable_value);
                        if !v.is_null() && len > 0 {
                            (*pg_stmt).param_values[pg_idx as usize] =
                                libc::malloc(len as usize) as *mut c_char;
                            if !(*pg_stmt).param_values[pg_idx as usize].is_null() {
                                libc::memcpy(
                                    (*pg_stmt).param_values[pg_idx as usize] as *mut c_void,
                                    v,
                                    len as usize,
                                );
                                if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
                                    crate::pg_mem_telemetry::rust_mem_telemetry_add(
                                        PMT_BIND_VALUE_BLOB_ALLOC,
                                        len as u64,
                                        1,
                                    );
                                }
                            }
                            (*pg_stmt).param_lengths[pg_idx as usize] = len;
                            (*pg_stmt).param_formats[pg_idx as usize] = 1;
                        }
                    }
                    SQLITE_NULL | _ => {
                        // Leave as NULL.
                    }
                }
            }
        }
    }

    drop(guard);
    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_null(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    note_bind_phase(PHASE_BIND_NULL, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) })
    } else {
        None
    };

    unsafe {
        clear_metadata_result_if_needed(pg_stmt);
        ensure_stmt_not_busy(p_stmt, pg_stmt);
    }

    let mut rc = unsafe { orig_sqlite3_bind_null.map(|f| f(p_stmt, idx)).unwrap_or(SQLITE_ERROR) };
    unsafe {
        rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
            orig_sqlite3_bind_null.map(|f| f(p_stmt, idx)).unwrap_or(SQLITE_ERROR)
        });
    }

    if !pg_stmt.is_null() && idx > 0 && idx <= MAX_PARAMS as c_int {
        let pg_idx = unsafe { pg_map_param_index(pg_stmt, p_stmt, idx) };
        if pg_idx >= 0 && (pg_idx as usize) < MAX_PARAMS {
            unsafe {
                if !(*pg_stmt).param_values[pg_idx as usize].is_null()
                    && !is_preallocated_buffer(pg_stmt, pg_idx as usize)
                {
                    libc::free((*pg_stmt).param_values[pg_idx as usize] as *mut c_void);
                    (*pg_stmt).param_values[pg_idx as usize] = ptr::null_mut();
                }
            }
        }
    }

    drop(guard);
    rc
}
