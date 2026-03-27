use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::{log_debug, log_info, PthreadMutexGuard};
use crate::ffi_types::{sqlite3_stmt, PgStmt, MAX_PARAMS, PARAM_BUF_LEN};

use super::ring_tracker::{
    clear_finalized_entry, clear_prepared_stmt, is_prepared_stmt, is_recently_finalized_stmt,
    log_clear_bindings_anomaly, remember_finalized_stmt, remember_prepared_stmt,
    skip_clear_bindings_on_finalized,
};
use super::*;
use crate::log_debug_lazy;

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

unsafe fn clear_dynamic_param_values(stmt: *mut PgStmt) {
    for i in 0..MAX_PARAMS {
        if !(*stmt).param_values[i].is_null() && !is_preallocated_buffer(stmt, i) {
            libc::free((*stmt).param_values[i] as *mut c_void);
            (*stmt).param_values[i] = ptr::null_mut();
        }
    }
}

unsafe fn reset_pg_stmt_locked(p_stmt: *mut sqlite3_stmt, stmt: *mut PgStmt) -> c_int {
    let _guard = PthreadMutexGuard::lock(&mut (*stmt).mutex as *mut _);
    (*stmt).in_step.store(0, Ordering::Relaxed);

    clear_dynamic_param_values(stmt);
    pg_stmt_clear_result(stmt);

    if (*stmt).is_pg != 2 {
        return orig_sqlite3_reset
            .map(|f| f(p_stmt))
            .unwrap_or(SQLITE_ERROR);
    }
    SQLITE_OK
}

pub(super) fn note_stmt_prepare_impl(p_stmt: *mut sqlite3_stmt, sql: *const c_char) {
    unsafe {
        remember_prepared_stmt(p_stmt, sql);
        clear_finalized_entry(p_stmt);
    }
}

pub(super) fn reset_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    if !pg_stmt.is_null() {
        return unsafe { reset_pg_stmt_locked(p_stmt, pg_stmt) };
    }

    let cached = unsafe { pg_find_cached_stmt(p_stmt) };
    if !cached.is_null() {
        return unsafe { reset_pg_stmt_locked(p_stmt, cached) };
    }

    unsafe {
        orig_sqlite3_reset
            .map(|f| f(p_stmt))
            .unwrap_or(SQLITE_ERROR)
    }
}

pub(super) fn finalize_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    unsafe {
        if skip_clear_bindings_on_finalized() && is_recently_finalized_stmt(p_stmt) {
            log_clear_bindings_anomaly("finalize on recently finalized", p_stmt);
            clear_prepared_stmt(p_stmt);
            return SQLITE_OK;
        }
        if !is_prepared_stmt(p_stmt) {
            log_clear_bindings_anomaly("finalize on unknown stmt", p_stmt);
        }

        let mut is_pg_only = 0;
        let mut is_pg_value = 0;
        let mut final_sql: *const c_char = ptr::null();

        let pg_stmt = pg_find_stmt(p_stmt);
        if !pg_stmt.is_null() {
            is_pg_value = (*pg_stmt).is_pg;
            is_pg_only = if (*pg_stmt).is_pg == 2 { 1 } else { 0 };
            final_sql = if !(*pg_stmt).pg_sql.is_null() {
                (*pg_stmt).pg_sql
            } else {
                (*pg_stmt).sql
            };

            let cached = pg_find_cached_stmt(p_stmt);
            if cached == pg_stmt {
                log_debug("finalize: stmt in both global and TLS, clearing TLS ref");
                pg_clear_cached_stmt(p_stmt);
            } else if !cached.is_null() {
                log_info("finalize: different pg_stmt in global vs TLS for same sqlite_stmt (cross-thread re-prepare)");
                pg_clear_cached_stmt(p_stmt);
            }

            pg_unregister_stmt(p_stmt);
            pg_stmt_unref(pg_stmt);
        } else {
            let cached = pg_find_cached_stmt(p_stmt);
            if !cached.is_null() {
                is_pg_value = (*cached).is_pg;
                is_pg_only = if (*cached).is_pg == 2 { 1 } else { 0 };
                if final_sql.is_null() {
                    final_sql = if !(*cached).pg_sql.is_null() {
                        (*cached).pg_sql
                    } else {
                        (*cached).sql
                    };
                }
                log_debug_lazy!(
                    "finalize: stmt only in TLS (ref_count={}), clearing",
                    (*cached).ref_count.load(Ordering::Relaxed)
                );
                pg_clear_cached_stmt(p_stmt);
                pg_stmt_unref(cached);
            }
        }

        if final_sql.is_null() {
            if let Some(f) = orig_sqlite3_sql {
                final_sql = f(p_stmt);
            }
        }

        let mut rc = SQLITE_OK;
        if is_pg_only == 0 {
            rc = orig_sqlite3_finalize
                .map(|f| f(p_stmt))
                .unwrap_or(SQLITE_ERROR);
        }
        clear_prepared_stmt(p_stmt);
        remember_finalized_stmt(p_stmt, final_sql, is_pg_value);
        rc
    }
}

pub(super) fn clear_bindings_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    unsafe {
        if skip_clear_bindings_on_finalized() && is_recently_finalized_stmt(p_stmt) {
            log_clear_bindings_anomaly("recently finalized", p_stmt);
            return SQLITE_OK;
        }

        let pg_stmt = pg_find_stmt(p_stmt);
        if pg_stmt.is_null() {
            log_clear_bindings_anomaly("stmt not registered", p_stmt);
        }

        if !pg_stmt.is_null() {
            let _guard = PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _);
            clear_dynamic_param_values(pg_stmt);
            if (*pg_stmt).is_pg == 0 {
                return orig_sqlite3_clear_bindings
                    .map(|f| f(p_stmt))
                    .unwrap_or(SQLITE_ERROR);
            }
            return SQLITE_OK;
        }

        orig_sqlite3_clear_bindings
            .map(|f| f(p_stmt))
            .unwrap_or(SQLITE_ERROR)
    }
}
