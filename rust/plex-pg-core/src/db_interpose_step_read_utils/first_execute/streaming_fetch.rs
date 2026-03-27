use super::*;
use crate::log_debug_lazy;

unsafe fn clear_streaming_state(pg_stmt: *mut PgStmt) {
    (*pg_stmt).streaming_mode = 0;
    if !(*pg_stmt).streaming_conn.is_null() {
        (*(*pg_stmt).streaming_conn)
            .streaming_active
            .store(0, Ordering::SeqCst);
    }
    (*pg_stmt).streaming_conn = std::ptr::null_mut();
}

unsafe fn finish_streaming_done(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    exec_conn: *mut PgConnection,
    stmt_guard: &mut PthreadMutexGuard,
) -> c_int {
    (*pg_stmt).read_done = 1;
    *exec_conn_io = exec_conn;
    stmt_guard.unlock();
    STEP_RESULT_DONE
}

pub(super) unsafe fn streaming_fetch_result(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    exec_conn: *mut PgConnection,
    stmt_guard: &mut PthreadMutexGuard,
    conn_guard: &mut PthreadMutexGuard,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    (*pg_stmt).streaming_mode = 1;
    (*pg_stmt).streaming_conn = exec_conn;
    (*pg_stmt).result_conn = exec_conn;
    (*exec_conn).streaming_active.store(1, Ordering::SeqCst);
    (*pg_stmt).metadata_only_result = 0;
    conn_guard.unlock();

    let first_res = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
    if first_res.is_null() {
        clear_streaming_state(pg_stmt);
        return finish_streaming_done(pg_stmt, exec_conn_io, exec_conn, stmt_guard);
    }

    let first_status = crate::libpq_helpers::rust_pq_result_status(first_res);
    if first_status == PGRES_SINGLE_TUPLE {
        // Clear any pre-existing result (e.g. from metadata-only fetch) to prevent leak
        if !(*pg_stmt).result.is_null() {
            crate::libpq_helpers::rust_pq_clear((*pg_stmt).result);
        }
        (*pg_stmt).result = first_res;
        (*pg_stmt).current_row = 0;
        (*pg_stmt).num_rows = 1;
        (*pg_stmt).num_cols = crate::libpq_helpers::rust_pq_nfields(first_res);
        (*pg_stmt).ensure_column_capacity((*pg_stmt).num_cols as usize);
        resolve_column_tables(pg_stmt, exec_conn);
        trace_play_queue_result(pg_stmt, first_res, "STREAM FIRST");
        *exec_conn_io = exec_conn;
        stmt_guard.unlock();
        return STEP_RESULT_ROW;
    }

    if first_status == PGRES_TUPLES_OK {
        log_debug_lazy!(
            "STREAM: zero rows returned for sql={:.200}",
            cstr_to_str((*pg_stmt).pg_sql)
        );
        crate::libpq_helpers::rust_pq_clear(first_res);
        let final_null = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
        if !final_null.is_null() {
            crate::libpq_helpers::rust_pq_clear(final_null);
        }
        clear_streaming_state(pg_stmt);
        (*pg_stmt).num_cols = 0;
        (*pg_stmt).num_rows = 0;
        return finish_streaming_done(pg_stmt, exec_conn_io, exec_conn, stmt_guard);
    }

    let err = crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn);
    log_error(&format!(
        "STREAM first fetch error: {} (status={}) sql={:.200}",
        cstr_to_str(err),
        first_status,
        cstr_to_str((*pg_stmt).pg_sql)
    ));
    if is_stale_prepared_stmt(first_res) {
        crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
    }
    crate::libpq_helpers::rust_pq_clear(first_res);
    let mut drain = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
    while !drain.is_null() {
        crate::libpq_helpers::rust_pq_clear(drain);
        drain = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
    }
    clear_streaming_state(pg_stmt);
    crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
    *exec_conn_io = exec_conn;
    stmt_guard.unlock();
    set_pg_conn_error(pg_conn_error_out);
    STEP_RESULT_ERROR
}
