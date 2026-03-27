use super::*;
use crate::log_debug_lazy;

pub(crate) fn should_clear_cross_thread_result(
    stmt: *const PgStmt,
    exec_conn: *mut PgConnection,
) -> bool {
    if stmt.is_null() || exec_conn.is_null() {
        return false;
    }
    unsafe {
        if (*stmt).result_conn == exec_conn {
            return false;
        }
        (*stmt).streaming_mode != 0
    }
}

pub(crate) fn should_use_streaming(stmt: *const PgStmt, disable_streaming_env: bool) -> bool {
    if disable_streaming_env || stmt.is_null() {
        return false;
    }
    unsafe {
        if (*stmt).needs_requery != 0 {
            return false;
        }

        let pg_sql = cstr_bytes((*stmt).pg_sql);
        if contains_icase_bytes(pg_sql, b"limit 1") {
            return false;
        }

        let sql = cstr_bytes((*stmt).sql);
        if contains_icase_bytes(sql, b"limit 1") {
            return false;
        }

        true
    }
}

pub(crate) unsafe fn adopt_materialized_result_owner(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) -> bool {
    if stmt.is_null()
        || exec_conn.is_null()
        || (*stmt).result.is_null()
        || (*stmt).streaming_mode != 0
        || (*stmt).result_conn == exec_conn
    {
        return false;
    }

    (*stmt).result_conn = exec_conn;
    (*stmt).executing_thread = libc::pthread_self();
    true
}

#[no_mangle]
pub extern "C" fn rust_step_read_prepare_reexecution_state(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) {
    if stmt.is_null() {
        return;
    }
    unsafe {
        let _stmt_guard = PthreadMutexGuard::lock(&mut (*stmt).mutex as *mut _);
        if should_clear_cross_thread_result(stmt, exec_conn) {
            (*stmt).needs_requery = 1;
            log_debug_lazy!(
                "STEP: Streaming stmt crossed threads; forcing eager requery (result_conn={:p} exec_conn={:p})",
                (*stmt).result_conn,
                exec_conn
            );
            crate::pg_statement::rust_stmt_clear_result(stmt);
        } else if adopt_materialized_result_owner(stmt, exec_conn) {
            log_debug_lazy!(
                "STEP: Reusing materialized eager result across threads (result_conn={:p} exec_conn={:p})",
                (*stmt).result_conn,
                exec_conn
            );
        }

        if !(*stmt).result.is_null() && (*stmt).metadata_only_result != 0 {
            log_debug("STEP: Clearing metadata-only result for re-execution");
            crate::libpq_helpers::rust_pq_clear((*stmt).result);
            (*stmt).result = std::ptr::null_mut();
            (*stmt).metadata_only_result = 0;
            (*stmt).current_row = -1;
        }
    }
}
