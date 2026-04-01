use super::*;
use crate::db_interpose_bind::support::{
    begin_bind, bytes_to_pg_hex, contains_binary_bytes, free_dynamic_param_value,
    invoke_destructor_if_custom, is_pg_routed_noncached, mapped_param_index, retry_on_misuse,
};
use crate::log_debug_lazy;

unsafe fn store_text_param(
    pg_stmt: *mut PgStmt,
    pg_idx: usize,
    val: *const c_char,
    actual_len: usize,
    duplicate_input: bool,
    idx: c_int,
    label: &str,
) {
    free_dynamic_param_value(pg_stmt, pg_idx);
    let stmt = &mut *pg_stmt;

    if contains_binary_bytes(val as *const u8, actual_len) {
        log_debug_lazy!(
            "{}: detected binary data at idx={}, len={}, converting to hex",
            label,
            idx,
            actual_len
        );
        stmt.param_values[pg_idx] = bytes_to_pg_hex(val as *const u8, actual_len);
        return;
    }

    if duplicate_input {
        stmt.param_values[pg_idx] = libc::strdup(val);
        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                PMT_BIND_TEXT_ALLOC,
                actual_len as u64 + 1,
                1,
            );
        }
        return;
    }

    stmt.param_values[pg_idx] = libc::malloc(actual_len + 1) as *mut c_char;
    if !stmt.param_values[pg_idx].is_null() {
        libc::memcpy(
            stmt.param_values[pg_idx] as *mut c_void,
            val as *const c_void,
            actual_len,
        );
        *stmt.param_values[pg_idx].add(actual_len) = 0;
        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                PMT_BIND_TEXT_ALLOC,
                actual_len as u64 + 1,
                1,
            );
        }
    }
}

unsafe fn store_blob_hex_param(
    pg_stmt: *mut PgStmt,
    pg_idx: usize,
    val: *const c_void,
    n_bytes: usize,
    idx: c_int,
    label: &str,
) {
    free_dynamic_param_value(pg_stmt, pg_idx);
    let stmt = &mut *pg_stmt;
    log_debug_lazy!(
        "{}: converting {} bytes to hex at idx={}",
        label,
        n_bytes,
        idx
    );
    stmt.param_values[pg_idx] = bytes_to_pg_hex(val as *const u8, n_bytes);
    stmt.param_lengths[pg_idx] = 0;
    stmt.param_formats[pg_idx] = 0;
}

pub(super) fn bind_text_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_TEXT, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        // Skip orig_sqlite3_bind_text — PG param storage below is sufficient.
        // Invoke custom destructor since SQLite won't do it.
        if !val.is_null() {
            unsafe { invoke_destructor_if_custom(val as *const c_void, destructor) };
        }
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_text()
            .map(|f| unsafe { f(p_stmt, idx, val, n_bytes, destructor) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_text()
                    .map(|f| f(p_stmt, idx, val, n_bytes, destructor))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if !val.is_null() {
        if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
            let actual_len = if n_bytes < 0 {
                unsafe { libc::strlen(val) as usize }
            } else {
                n_bytes as usize
            };
            unsafe {
                store_text_param(
                    pg_stmt,
                    pg_idx,
                    val,
                    actual_len,
                    n_bytes < 0,
                    idx,
                    "bind_text",
                );
            }
        }
    }

    drop(guard);
    crate::pg_mem_telemetry::rust_mem_telemetry_maybe_log();
    rc
}

pub(super) fn bind_blob_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_BLOB, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        if !val.is_null() {
            unsafe { invoke_destructor_if_custom(val, destructor) };
        }
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_blob()
            .map(|f| unsafe { f(p_stmt, idx, val, n_bytes, destructor) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_blob()
                    .map(|f| f(p_stmt, idx, val, n_bytes, destructor))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if !val.is_null() && n_bytes > 0 {
        if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
            unsafe {
                store_blob_hex_param(pg_stmt, pg_idx, val, n_bytes as usize, idx, "bind_blob");
            }
        }
    }

    drop(guard);
    rc
}

pub(super) fn bind_blob64_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: u64,
    destructor: *mut c_void,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_BLOB64, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        if !val.is_null() {
            unsafe { invoke_destructor_if_custom(val, destructor) };
        }
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_blob64()
            .map(|f| unsafe { f(p_stmt, idx, val, n_bytes, destructor) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_blob64()
                    .map(|f| f(p_stmt, idx, val, n_bytes, destructor))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if !val.is_null() && n_bytes > 0 {
        if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
            unsafe {
                store_blob_hex_param(pg_stmt, pg_idx, val, n_bytes as usize, idx, "bind_blob64")
            };
        }
    }

    drop(guard);
    rc
}

pub(super) fn bind_text64_impl(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n_bytes: u64,
    destructor: *mut c_void,
    encoding: c_uchar,
) -> c_int {
    let (pg_stmt, guard) = unsafe { begin_bind(PHASE_BIND_TEXT64, p_stmt) };

    let rc = if is_pg_routed_noncached(pg_stmt) {
        if !val.is_null() {
            unsafe { invoke_destructor_if_custom(val as *const c_void, destructor) };
        }
        SQLITE_OK
    } else {
        let mut rc = get_orig_sqlite3_bind_text64()
            .map(|f| unsafe { f(p_stmt, idx, val, n_bytes, destructor, encoding) })
            .unwrap_or(SQLITE_ERROR);
        unsafe {
            rc = retry_on_misuse(rc, p_stmt, pg_stmt, || {
                get_orig_sqlite3_bind_text64()
                    .map(|f| f(p_stmt, idx, val, n_bytes, destructor, encoding))
                    .unwrap_or(SQLITE_ERROR)
            });
        }
        rc
    };

    if !val.is_null() {
        if let Some(pg_idx) = unsafe { mapped_param_index(pg_stmt, p_stmt, idx) } {
            let actual_len = if n_bytes == u64::MAX {
                unsafe { libc::strlen(val) as usize }
            } else {
                n_bytes as usize
            };
            unsafe {
                store_text_param(
                    pg_stmt,
                    pg_idx,
                    val,
                    actual_len,
                    n_bytes == u64::MAX,
                    idx,
                    "bind_text64",
                );
            }
        }
    }

    drop(guard);
    crate::pg_mem_telemetry::rust_mem_telemetry_maybe_log();
    rc
}
