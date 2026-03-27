use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::Ordering;

use crate::db_interpose_common::tls_in_resolve_tables_ptr;
use crate::db_interpose_conn_utils::{cstr_prefix, PthreadMutexGuard};
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt, MAX_PARAMS};
use crate::log_debug_lazy;

mod cached;
mod retry;
mod support;

pub(crate) const SQLITE_DONE: c_int = 101;
pub(crate) const SQLITE_ROW: c_int = 100;
pub(crate) const SQLITE_ERROR: c_int = 1;

pub(crate) const STEP_RESULT_FALLBACK: c_int = -1;
pub(crate) const STEP_RESULT_DONE: c_int = SQLITE_DONE;
pub(crate) const STEP_RESULT_ROW: c_int = SQLITE_ROW;
pub(crate) const STEP_RESULT_ERROR: c_int = SQLITE_ERROR;

const PQTRANS_IDLE: c_int = 0;

use cached::step_handle_cached_stmt;
use retry::{maybe_retry_step, note_pg_conn_error};
use support::{
    call_sqlite3_db_handle, call_sqlite3_sql, orig_step, pg_exception_note_phase,
    shim_alloc_maybe_log,
};

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_step(p_stmt: *mut sqlite3_stmt) -> c_int {
    let dbg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    let mut dbg_sql: *const c_char = std::ptr::null();
    let dbg_db: *mut sqlite3;

    unsafe {
        if !dbg_stmt.is_null() {
            dbg_sql = if !(*dbg_stmt).pg_sql.is_null() {
                (*dbg_stmt).pg_sql
            } else {
                (*dbg_stmt).sql
            };
        }
        if dbg_sql.is_null() {
            dbg_sql = call_sqlite3_sql(p_stmt);
        }
        dbg_db = call_sqlite3_db_handle(p_stmt);
    }

    let phase = b"step\0";
    unsafe {
        pg_exception_note_phase(phase.as_ptr() as *const c_char, dbg_sql, p_stmt, dbg_db);
    }

    let rc = unsafe { my_sqlite3_step_impl(p_stmt) };

    if let Some(retry_rc) = maybe_retry_step(p_stmt, rc) {
        return retry_rc;
    }

    rc
}

unsafe fn my_sqlite3_step_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    shim_alloc_maybe_log();

    if *tls_in_resolve_tables_ptr() != 0 {
        return orig_step(p_stmt);
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;

    if !pg_stmt.is_null() {
        (*pg_stmt).in_step.store(1, Ordering::SeqCst);
    }

    if !pg_stmt.is_null() && (*pg_stmt).is_pg == 3 {
        log_debug_lazy!(
            "[RACE_DEBUG] STEP_END thread={:p} stmt={:p} rc={} reason=skip",
            libc::pthread_self() as *mut c_void,
            p_stmt,
            SQLITE_DONE
        );
        return SQLITE_DONE;
    }

    if pg_stmt.is_null() {
        let cached_rc = step_handle_cached_stmt(p_stmt);
        if cached_rc != STEP_RESULT_FALLBACK {
            return cached_rc;
        }
    }

    let mut exec_conn: *mut PgConnection = std::ptr::null_mut();

    if !pg_stmt.is_null() && !(*pg_stmt).shadow_stmt.is_null() {
        let db = call_sqlite3_db_handle((*pg_stmt).shadow_stmt);
        let handle_conn = crate::pg_client::rust_pg_find_connection(db);
        if !handle_conn.is_null()
            && (*handle_conn).is_pg_active != 0
            && crate::db_interpose_helpers::rust_is_library_or_blobs_db_path(
                (*handle_conn).db_path.as_ptr(),
            ) != 0
        {
            if !(*handle_conn).conn.is_null() {
                exec_conn = handle_conn;
            } else {
                let thread_conn =
                    crate::pg_client::rust_pool_get_connection((*handle_conn).db_path.as_ptr())
                        as *mut PgConnection;
                if !thread_conn.is_null()
                    && (*thread_conn).is_pg_active != 0
                    && !(*thread_conn).conn.is_null()
                {
                    exec_conn = thread_conn;
                    crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
                }
            }
        }
    }

    if !pg_stmt.is_null()
        && !(*pg_stmt).pg_sql.is_null()
        && !exec_conn.is_null()
        && !(*exec_conn).conn.is_null()
    {
        let mut param_values: [*const c_char; MAX_PARAMS] = [std::ptr::null(); MAX_PARAMS];
        let (stmt_is_pg, read_done, has_cached_result, write_executed, pg_sql) = {
            let _stmt_guard = PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _);
            let max_params = (*pg_stmt).param_count.min(MAX_PARAMS as c_int);
            for i in 0..max_params {
                param_values[i as usize] = (*pg_stmt).param_values[i as usize] as *const c_char;
            }
            (
                (*pg_stmt).is_pg,
                (*pg_stmt).read_done != 0,
                !(*pg_stmt).cached_result.is_null(),
                (*pg_stmt).write_executed != 0,
                (*pg_stmt).pg_sql,
            )
        };

        if stmt_is_pg == 2 {
            if read_done {
                return SQLITE_DONE;
            }

            if has_cached_result {
                return crate::db_interpose_step_read_utils::rust_step_read_advance_cached_result(
                    pg_stmt,
                );
            }

            crate::db_interpose_step_read_utils::rust_step_read_log_debug_context(
                pg_stmt, exec_conn,
            );
            crate::db_interpose_step_read_utils::rust_step_read_prepare_reexecution_state(
                pg_stmt, exec_conn,
            );

            let (read_done, has_cached_result, streaming_mode, has_result) = {
                let _stmt_guard = PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _);
                (
                    (*pg_stmt).read_done != 0,
                    !(*pg_stmt).cached_result.is_null(),
                    (*pg_stmt).streaming_mode != 0,
                    !(*pg_stmt).result.is_null(),
                )
            };

            if read_done {
                return SQLITE_DONE;
            }

            if has_cached_result {
                return crate::db_interpose_step_read_utils::rust_step_read_advance_cached_result(
                    pg_stmt,
                );
            }

            if streaming_mode {
                return crate::db_interpose_step_read_utils::rust_step_read_streaming_next(
                    p_stmt, pg_stmt,
                );
            }

            if has_result {
                return crate::db_interpose_step_read_utils::rust_step_read_eager_next(pg_stmt);
            }

            let mut conn_error = 0;
            let first_rc = crate::db_interpose_step_read_utils::rust_step_read_first_execute(
                pg_stmt,
                &mut exec_conn,
                param_values.as_ptr(),
                &mut conn_error,
            );
            if first_rc == STEP_RESULT_ERROR && conn_error != 0 {
                note_pg_conn_error();
            }
            return first_rc;
        } else if stmt_is_pg == 1 {
            if write_executed {
                return SQLITE_DONE;
            }

            let mut txn_state = PQTRANS_IDLE;
            if crate::db_interpose_step_write_utils::rust_step_pg_write_should_noop(
                exec_conn,
                pg_sql,
                &mut txn_state,
            ) != 0
            {
                log_debug_lazy!(
                    "TXN_NOOP: skipping tx terminator in state={} sql={}",
                    txn_state,
                    cstr_prefix(pg_sql, 120, "(null)")
                );
                let _stmt_guard = PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _);
                (*pg_stmt).write_executed = 1;
                return SQLITE_DONE;
            }

            crate::db_interpose_step_write_utils::rust_step_write_log_debug_context(
                pg_stmt,
                exec_conn,
                param_values.as_ptr(),
            );

            if crate::db_interpose_step_write_utils::rust_step_write_should_skip_special_insert(
                pg_stmt,
                exec_conn,
                param_values.as_ptr(),
            ) != 0
            {
                return SQLITE_DONE;
            }

            let mut prep_conn_error = 0;
            let prep_rc = crate::db_interpose_step_write_utils::rust_step_write_prepare_connection(
                pg_stmt,
                &mut exec_conn,
                &mut prep_conn_error,
            );
            if prep_rc == STEP_RESULT_ERROR {
                if prep_conn_error != 0 {
                    note_pg_conn_error();
                }
                return SQLITE_ERROR;
            }

            let mut write_conn_error = 0;
            let write_rc =
                crate::db_interpose_step_write_utils::rust_step_write_execute_and_finalize(
                    pg_stmt,
                    exec_conn,
                    param_values.as_ptr(),
                    &mut write_conn_error,
                );
            if write_rc == STEP_RESULT_ERROR {
                if write_conn_error != 0 {
                    note_pg_conn_error();
                }
                return SQLITE_ERROR;
            }
        }
    }

    if !pg_stmt.is_null() && (*pg_stmt).is_pg != 0 {
        if (*pg_stmt).is_pg == 1 {
            return SQLITE_DONE;
        }
        crate::db_interpose_step_write_utils::rust_step_log_step_exit_trace(pg_stmt);
    }

    orig_step(p_stmt)
}
