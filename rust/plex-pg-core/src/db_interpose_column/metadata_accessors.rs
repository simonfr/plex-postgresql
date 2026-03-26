use super::*;

pub(super) fn column_count_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        // Check if we need metadata BEFORE acquiring stmt mutex to avoid
        // ABBA deadlock (stmt mutex -> conn mutex).
        let needs_metadata = unsafe {
            (*pg_stmt).num_cols == 0
                && !(*pg_stmt).pg_sql.is_null()
                && (*pg_stmt).result.is_null()
                && (*pg_stmt).cached_result.is_null()
        };
        if needs_metadata {
            ensure_pg_result_for_metadata(pg_stmt);
        }

        // Hold mutex only for data reads — no logging or nested locks.
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };
        if !unsafe { (*pg_stmt).cached_result }.is_null() {
            let cached = unsafe { &*(*pg_stmt).cached_result };
            return cached.num_cols;
        }
        return unsafe { (*pg_stmt).num_cols };
    }
    unsafe { orig_sqlite3_column_count.map(|f| f(p_stmt)).unwrap_or(0) }
}

pub(super) fn column_name_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    let mut result: *const c_char = ptr::null();
    let mut use_orig = true;

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        // Call ensure_pg_result_for_metadata BEFORE acquiring stmt mutex to
        // avoid ABBA deadlock (stmt mutex -> conn mutex).
        let needs_metadata = unsafe {
            (*pg_stmt).result.is_null()
                && (*pg_stmt).cached_result.is_null()
                && (*pg_stmt).col_names.is_null()
                && !(*pg_stmt).pg_sql.is_null()
        };
        if needs_metadata {
            ensure_pg_result_for_metadata(pg_stmt);
        }

        // Hold mutex only for data reads — no logging inside this block
        // to avoid ABBA deadlock between stmt mutex and LOGGER mutex.
        {
            let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

            if !unsafe { (*pg_stmt).col_names.is_null() }
                && idx >= 0
                && idx < unsafe { (*pg_stmt).num_col_names }
            {
                result = unsafe { *(*pg_stmt).col_names.add(idx as usize) };
                use_orig = false;
            } else if !unsafe { (*pg_stmt).result.is_null() }
                && idx >= 0
                && idx < unsafe { (*pg_stmt).num_cols }
            {
                result = unsafe {
                    crate::db_interpose_helpers::rust_pg_result_col_name(
                        helpers_result_ptr((*pg_stmt).result),
                        idx,
                    )
                };
                use_orig = false;
            } else {
                use_orig = unsafe { (*pg_stmt).result.is_null() }
                    && unsafe { (*pg_stmt).col_names.is_null() };
            }
        }
        // Mutex released here.
    }

    if use_orig {
        result = unsafe {
            orig_sqlite3_column_name
                .map(|f| f(p_stmt, idx))
                .unwrap_or(ptr::null())
        };
    }
    result
}

pub(super) fn data_count_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        // Hold mutex only for data reads — no logging inside this block
        // to avoid ABBA deadlock between stmt mutex and LOGGER mutex.
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };
        let count = if unsafe { (*pg_stmt).current_row } < unsafe { (*pg_stmt).num_rows } {
            unsafe { (*pg_stmt).num_cols }
        } else {
            0
        };
        return count;
    }

    unsafe { orig_sqlite3_data_count.map(|f| f(p_stmt)).unwrap_or(0) }
}
