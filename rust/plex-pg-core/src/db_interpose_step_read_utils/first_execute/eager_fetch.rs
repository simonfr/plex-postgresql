use super::*;

pub(super) unsafe fn eager_fetch_result(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    exec_conn: *mut PgConnection,
    stmt_guard: &mut PthreadMutexGuard,
    conn_guard: &mut PthreadMutexGuard,
) -> c_int {
    // Clear any pre-existing result (e.g. from metadata-only fetch) to prevent leak
    if !(*pg_stmt).result.is_null() {
        crate::libpq_helpers::rust_pq_clear((*pg_stmt).result);
    }
    (*pg_stmt).result = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
    let mut trail = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
    while !trail.is_null() {
        crate::libpq_helpers::rust_pq_clear(trail);
        trail = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
    }
    conn_guard.unlock();

    if !(*pg_stmt).result.is_null()
        && crate::libpq_helpers::rust_pq_result_status((*pg_stmt).result) == PGRES_TUPLES_OK
    {
        (*pg_stmt).num_rows = crate::libpq_helpers::rust_pq_ntuples((*pg_stmt).result);
        (*pg_stmt).num_cols = crate::libpq_helpers::rust_pq_nfields((*pg_stmt).result);
        (*pg_stmt).ensure_column_capacity((*pg_stmt).num_cols as usize);
        (*pg_stmt).current_row = 0;
        (*pg_stmt).result_conn = exec_conn;
        (*pg_stmt).metadata_only_result = 0;
        resolve_column_tables(pg_stmt, exec_conn);
        trace_play_queue_result(pg_stmt, (*pg_stmt).result, "EAGER");
        if (*pg_stmt).num_rows > 0 {
            *exec_conn_io = exec_conn;
            stmt_guard.unlock();
            return STEP_RESULT_ROW;
        }
    } else if !(*pg_stmt).result.is_null() {
        let err2 = crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn);
        let ctx = CString::new("EAGER FALLBACK").unwrap();
        let fallback_err = if err2.is_null() {
            CString::new("?").unwrap()
        } else {
            CString::new(cstr_to_str(err2)).unwrap_or_else(|_| CString::new("?").unwrap())
        };
        log_sql_fallback(
            (*pg_stmt).sql,
            (*pg_stmt).pg_sql,
            fallback_err.as_ptr(),
            ctx.as_ptr(),
        );
        crate::libpq_helpers::rust_pq_clear((*pg_stmt).result);
        (*pg_stmt).result = std::ptr::null_mut();
        crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
    }

    (*pg_stmt).read_done = 1;
    *exec_conn_io = exec_conn;
    stmt_guard.unlock();
    STEP_RESULT_DONE
}
