use super::*;
use crate::log_debug_lazy;

pub(super) unsafe fn pg_map_param_index(
    pg_stmt: *mut PgStmt,
    p_stmt: *mut sqlite3_stmt,
    sqlite_idx: c_int,
) -> c_int {
    if pg_stmt.is_null() {
        log_debug_lazy!(
            "pg_map_param_index: no pg_stmt, using direct mapping idx={} -> {}",
            sqlite_idx,
            sqlite_idx - 1
        );
        return sqlite_idx - 1;
    }

    if !(*pg_stmt).param_names.is_null() && (*pg_stmt).param_count > 0 {
        let param_name =
            crate::db_interpose_metadata::rust_my_sqlite3_bind_parameter_name(p_stmt, sqlite_idx);
        log_debug_lazy!(
            "pg_map_param_index: sqlite_idx={}, param_name={}, param_count={}",
            sqlite_idx,
            cstr_to_string_or(param_name, "NULL"),
            (*pg_stmt).param_count
        );

        if !param_name.is_null() {
            let mut clean_name = param_name;
            if *param_name == b':' as c_char {
                clean_name = param_name.add(1);
            }

            let param_count = (*pg_stmt).param_count as usize;
            let max_debug = param_count.min(5);
            for i in 0..max_debug {
                let cur = *(*pg_stmt).param_names.add(i);
                log_debug_lazy!(
                    "  param_names[{}] = {}",
                    i,
                    cstr_to_string_or(cur, "NULL")
                );
            }

            for i in 0..param_count {
                let cur = *(*pg_stmt).param_names.add(i);
                if !cur.is_null() && libc::strcmp(cur, clean_name) == 0 {
                    log_debug_lazy!("  -> Found match at pg_idx={}", i);
                    return i as c_int;
                }
            }
            log_debug_lazy!(
                "Named parameter '{}' not found in translation (sqlite_idx={})",
                cstr_to_string_or(clean_name, "NULL"),
                sqlite_idx
            );
        } else {
            log_debug("  -> No parameter name, using direct mapping");
        }
    } else {
        log_debug_lazy!(
            "pg_map_param_index: no param_names (count={}), using direct mapping idx={} -> {}",
            (*pg_stmt).param_count,
            sqlite_idx,
            sqlite_idx - 1
        );
    }

    sqlite_idx - 1
}

pub(super) fn note_bind_phase(phase: &[u8], p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) {
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
        pg_exception_note_phase(phase.as_ptr() as *const c_char, sql, p_stmt, db);
    }
}

pub(super) fn bind_reset_disabled() -> bool {
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

pub(super) fn contains_binary_bytes(data: *const u8, len: usize) -> bool {
    crate::db_interpose_helpers::rust_contains_binary_bytes(data, len) != 0
}

pub(super) unsafe fn bytes_to_pg_hex(data: *const u8, len: usize) -> *mut c_char {
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

pub(super) fn should_reset_stmt(pg_stmt: *mut PgStmt) -> bool {
    if bind_reset_disabled() || pg_stmt.is_null() {
        return false;
    }
    unsafe { (*pg_stmt).is_pg != 0 }
}

pub(super) unsafe fn wait_for_stmt_ready(p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) -> bool {
    if should_reset_stmt(pg_stmt) {
        if let Some(f) = orig_sqlite3_reset {
            f(p_stmt);
        }
    }
    libc::usleep(500);
    true
}

pub(super) unsafe fn ensure_stmt_not_busy(p_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) {
    if should_reset_stmt(pg_stmt) {
        if let Some(f) = orig_sqlite3_reset {
            f(p_stmt);
        }
    }
}

pub(super) unsafe fn clear_metadata_result_if_needed(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }
    if (*pg_stmt).metadata_only_result != 0 && !(*pg_stmt).result.is_null() {
        log_debug("BIND: Marking metadata-only result for re-execution with bound params");
        (*pg_stmt).metadata_only_result = 2;
    }
}

pub(super) unsafe fn is_preallocated_buffer(stmt: *mut PgStmt, idx: usize) -> bool {
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

pub(super) unsafe fn retry_on_misuse<F>(
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

pub(super) unsafe fn begin_bind(
    phase: &[u8],
    p_stmt: *mut sqlite3_stmt,
) -> (*mut PgStmt, Option<PthreadMutexGuard>) {
    let pg_stmt = pg_find_any_stmt(p_stmt);
    note_bind_phase(phase, p_stmt, pg_stmt);

    let guard = if !pg_stmt.is_null() {
        Some(PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _))
    } else {
        None
    };

    clear_metadata_result_if_needed(pg_stmt);
    ensure_stmt_not_busy(p_stmt, pg_stmt);
    (pg_stmt, guard)
}

pub(super) unsafe fn mapped_param_index(
    pg_stmt: *mut PgStmt,
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> Option<usize> {
    if pg_stmt.is_null() || idx <= 0 || idx > MAX_PARAMS as c_int {
        return None;
    }
    let pg_idx = pg_map_param_index(pg_stmt, p_stmt, idx);
    if pg_idx < 0 || (pg_idx as usize) >= MAX_PARAMS {
        return None;
    }
    Some(pg_idx as usize)
}

pub(super) unsafe fn free_dynamic_param_value(pg_stmt: *mut PgStmt, pg_idx: usize) {
    if !(*pg_stmt).param_values[pg_idx].is_null() && !is_preallocated_buffer(pg_stmt, pg_idx) {
        libc::free((*pg_stmt).param_values[pg_idx] as *mut c_void);
        (*pg_stmt).param_values[pg_idx] = ptr::null_mut();
    }
}
