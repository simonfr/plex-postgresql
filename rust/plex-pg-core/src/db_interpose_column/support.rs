use super::*;

#[inline]
pub(crate) fn helpers_result_ptr(result: *mut PgResultLibpq) -> *const PgResultHelpers {
    result as *const PgResultHelpers
}

pub(crate) fn sqlite_type_name(t: c_int) -> &'static str {
    match t {
        SQLITE_INTEGER => "INTEGER",
        SQLITE_FLOAT => "FLOAT",
        SQLITE_TEXT => "TEXT",
        SQLITE_BLOB => "BLOB",
        SQLITE_NULL => "NULL",
        _ => "UNKNOWN",
    }
}

pub(crate) fn next_text_buffer_index() -> usize {
    COLUMN_TEXT_BUF_IDX.with(|idx| {
        let cur = idx.get();
        idx.set((cur + 1) % NUM_TEXT_BUFFERS);
        cur
    })
}

pub(crate) fn validate_type_consistency(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    accessor_name: &str,
) {
    // Skip expensive validation unless debug logging is enabled.
    if crate::pg_logging::LOG_LEVEL.load(std::sync::atomic::Ordering::Relaxed) < 2 {
        return;
    }

    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    if pg_stmt.is_null() || unsafe { (*pg_stmt).is_pg == 0 } {
        return;
    }

    let col_type = rust_my_sqlite3_column_type(p_stmt, idx);
    let col_decltype = rust_my_sqlite3_column_decltype(p_stmt, idx);

    // Acquire mutex only to extract data; all logging happens after release.
    let mismatch_ctx = {
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };
        if unsafe { (*pg_stmt).result.is_null() } {
            return;
        }

        let oid = unsafe {
            crate::db_interpose_helpers::rust_pg_result_col_oid(
                helpers_result_ptr((*pg_stmt).result),
                idx,
            )
        };
        let col_name = unsafe {
            crate::db_interpose_helpers::rust_pg_result_col_name(
                helpers_result_ptr((*pg_stmt).result),
                idx,
            )
        };

        if col_decltype.is_null() {
            return;
        }
        let expected =
            crate::db_interpose_helpers::rust_expected_sqlite_type_for_decltype(col_decltype);
        if expected == -1 || col_type == SQLITE_NULL || col_type == expected {
            return;
        }

        let current_row = unsafe { (*pg_stmt).current_row };
        let pg_sql = unsafe { (*pg_stmt).pg_sql };
        let should_trace = trace_badcast_should_log(pg_stmt, idx);
        (oid, col_name, expected, current_row, pg_sql, should_trace)
    };

    // Log AFTER releasing pg_stmt.mutex to avoid ABBA deadlock with LOGGER mutex.
    let (oid, col_name, expected, current_row, pg_sql, should_trace) = mismatch_ctx;
    log_debug(&format!(
        "TYPE_MISMATCH: accessor={} col='{}' idx={} decltype='{}' expects {} but column_type returned {} (OID={})",
        accessor_name,
        cstr_to_string_or(col_name, "?"),
        idx,
        cstr_to_string_or(col_decltype, "?"),
        sqlite_type_name(expected),
        sqlite_type_name(col_type),
        oid
    ));

    if should_trace {
        trace_badcast_log_ctx(
            pg_stmt,
            p_stmt,
            idx,
            accessor_name,
            "type_mismatch",
            current_row,
            if col_type == SQLITE_NULL { 1 } else { 0 },
            oid,
            col_name,
        );
        log_debug(&format!(
            "TRACE_BADCAST_MISMATCH: accessor={} col='{}' idx={} oid={} decltype='{}' expected={} actual={} sql={}",
            accessor_name,
            cstr_to_string_or(col_name, "?"),
            idx,
            oid,
            cstr_to_string_or(col_decltype, "?"),
            sqlite_type_name(expected),
            sqlite_type_name(col_type),
            cstr_prefix(pg_sql, 200, "?")
        ));
    }
}
