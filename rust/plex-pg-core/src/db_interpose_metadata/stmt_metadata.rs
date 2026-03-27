use super::*;
use crate::log_debug_lazy;

fn lookup_pg_stmt(p_stmt: *mut sqlite3_stmt) -> *mut PgStmt {
    crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt
}

fn is_interposed_pg_stmt(pg_stmt: *mut PgStmt) -> bool {
    !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2
}

fn normalized_bind_name(name: *const c_char) -> *const c_char {
    unsafe {
        let first = *name as u8;
        if first == b':' || first == b'@' || first == b'$' {
            name.add(1)
        } else {
            name
        }
    }
}

pub(super) fn db_handle_impl(p_stmt: *mut sqlite3_stmt) -> *mut sqlite3 {
    log_debug_lazy!("DB_HANDLE: pStmt={:p}", p_stmt);
    if p_stmt.is_null() {
        return std::ptr::null_mut();
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        unsafe {
            if !(*pg_stmt).shadow_stmt.is_null() {
                if let Some(f) = orig_sqlite3_db_handle {
                    let db = f((*pg_stmt).shadow_stmt);
                    log_debug_lazy!("DB_HANDLE: returning from shadow_stmt={:p}", db);
                    return db;
                }
            }
            if !(*pg_stmt).conn.is_null() && !(*(*pg_stmt).conn).shadow_db.is_null() {
                log_debug_lazy!(
                    "DB_HANDLE: returning shadow_db={:p}",
                    (*(*pg_stmt).conn).shadow_db
                );
                return (*(*pg_stmt).conn).shadow_db;
            }
        }
        log_debug("DB_HANDLE: pg_stmt has no valid db handle");
        return std::ptr::null_mut();
    }

    unsafe {
        if let Some(f) = orig_sqlite3_db_handle {
            let db = f(p_stmt);
            log_debug_lazy!("DB_HANDLE: returning orig={:p}", db);
            return db;
        }
    }
    std::ptr::null_mut()
}

pub(super) fn sql_impl(p_stmt: *mut sqlite3_stmt) -> *const c_char {
    if p_stmt.is_null() {
        return std::ptr::null();
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        unsafe {
            return (*pg_stmt).sql;
        }
    }

    unsafe {
        if let Some(f) = orig_sqlite3_sql {
            return f(p_stmt);
        }
    }
    std::ptr::null()
}

pub(super) fn bind_parameter_count_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    if p_stmt.is_null() {
        return 0;
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        unsafe {
            return (*pg_stmt).param_count;
        }
    }

    unsafe {
        if let Some(f) = orig_sqlite3_bind_parameter_count {
            return f(p_stmt);
        }
    }
    0
}

pub(super) fn stmt_readonly_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    if p_stmt.is_null() {
        return 1;
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        unsafe {
            if !(*pg_stmt).sql.is_null() {
                return crate::pg_config::pg_config_is_read_operation((*pg_stmt).sql);
            }
        }
        return 1;
    }

    unsafe {
        if let Some(f) = orig_sqlite3_stmt_readonly {
            return f(p_stmt);
        }
    }
    1
}

pub(super) fn stmt_busy_impl(p_stmt: *mut sqlite3_stmt) -> c_int {
    log_debug_lazy!("STMT_BUSY: stmt={:p}", p_stmt);
    if p_stmt.is_null() {
        return 0;
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        unsafe {
            let busy = !(*pg_stmt).result.is_null() && (*pg_stmt).current_row < (*pg_stmt).num_rows;
            log_debug_lazy!(
                "STMT_BUSY: pg_stmt, result={:p} current_row={} num_rows={} -> busy={}",
                (*pg_stmt).result,
                (*pg_stmt).current_row,
                (*pg_stmt).num_rows,
                busy as i32
            );
            return busy as c_int;
        }
    }

    unsafe {
        if let Some(f) = orig_sqlite3_stmt_busy {
            return f(p_stmt);
        }
    }
    0
}

pub(super) fn stmt_status_impl(p_stmt: *mut sqlite3_stmt, op: c_int, reset: c_int) -> c_int {
    log_debug_lazy!(
        "STMT_STATUS: stmt={:p} op={} reset={}",
        p_stmt, op, reset
    );
    if p_stmt.is_null() {
        return 0;
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        log_debug("STMT_STATUS: pg_stmt returning 0");
        return 0;
    }

    unsafe {
        if let Some(f) = orig_sqlite3_stmt_status {
            return f(p_stmt, op, reset);
        }
    }
    0
}

pub(super) fn bind_parameter_name_impl(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    log_debug_lazy!("BIND_PARAM_NAME: stmt={:p} idx={}", p_stmt, idx);
    if p_stmt.is_null() {
        return std::ptr::null();
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        unsafe {
            if idx > 0 && idx <= (*pg_stmt).param_count && !(*pg_stmt).param_names.is_null() {
                let name = *(*pg_stmt).param_names.add((idx - 1) as usize);
                log_debug_lazy!(
                    "BIND_PARAM_NAME: pg_stmt returning '{}'",
                    cstr_to_string_or(name, "NULL")
                );
                return name;
            }
        }
        log_debug("BIND_PARAM_NAME: pg_stmt idx out of range, returning NULL");
        return std::ptr::null();
    }

    unsafe {
        if let Some(f) = orig_sqlite3_bind_parameter_name {
            return f(p_stmt, idx);
        }
    }
    std::ptr::null()
}

pub(super) fn bind_parameter_index_impl(p_stmt: *mut sqlite3_stmt, name: *const c_char) -> c_int {
    if p_stmt.is_null() || name.is_null() {
        return 0;
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        unsafe {
            if (*pg_stmt).param_names.is_null() || (*pg_stmt).param_count == 0 {
                log_debug_lazy!(
                    "BIND_PARAM_INDEX: pg_stmt has no params, falling through to SQLite for '{}'",
                    cstr_to_string_or(name, "")
                );
            } else {
                let name_to_find = normalized_bind_name(name);
                for i in 0..(*pg_stmt).param_count {
                    let cur = *(*pg_stmt).param_names.add(i as usize);
                    if !cur.is_null()
                        && !name_to_find.is_null()
                        && libc::strcmp(cur, name_to_find) == 0
                    {
                        log_debug_lazy!(
                            "BIND_PARAM_INDEX: found '{}' at index {}",
                            cstr_to_string_or(name, ""),
                            i + 1
                        );
                        return i + 1;
                    }
                }
                log_debug_lazy!(
                    "BIND_PARAM_INDEX: '{}' not found in pg_stmt (param_count={})",
                    cstr_to_string_or(name, ""),
                    (*pg_stmt).param_count
                );
                return 0;
            }
        }
    }

    unsafe {
        if let Some(f) = orig_sqlite3_bind_parameter_index {
            return f(p_stmt, name);
        }
    }
    0
}

pub(super) fn expanded_sql_impl(p_stmt: *mut sqlite3_stmt) -> *mut c_char {
    if p_stmt.is_null() {
        return std::ptr::null_mut();
    }

    let pg_stmt = lookup_pg_stmt(p_stmt);
    if is_interposed_pg_stmt(pg_stmt) {
        unsafe {
            let base_sql = if !(*pg_stmt).pg_sql.is_null() {
                (*pg_stmt).pg_sql
            } else {
                (*pg_stmt).sql
            };
            if base_sql.is_null() {
                return std::ptr::null_mut();
            }

            let base_len = CStr::from_ptr(base_sql).to_bytes().len();
            if (*pg_stmt).param_count == 0 {
                let result = super::rust_my_sqlite3_malloc((base_len + 1) as c_int) as *mut c_char;
                if result.is_null() {
                    return std::ptr::null_mut();
                }
                std::ptr::copy_nonoverlapping(base_sql, result, base_len);
                *result.add(base_len) = 0;
                return result;
            }

            let mut estimated = base_len + 1;
            for i in 0..(*pg_stmt).param_count.min(MAX_PARAMS as c_int) {
                let val = (*pg_stmt).param_values[i as usize];
                if !val.is_null() {
                    estimated += CStr::from_ptr(val).to_bytes().len() + 3;
                } else {
                    estimated += 4;
                }
            }
            estimated = estimated.saturating_mul(2);

            let result = super::rust_my_sqlite3_malloc(estimated as c_int) as *mut c_char;
            if result.is_null() {
                return std::ptr::null_mut();
            }

            let src = CStr::from_ptr(base_sql).to_bytes();
            let mut dst = result as *mut u8;
            let end = result.add(estimated - 1) as *mut u8;
            let mut idx = 0usize;

            while idx < src.len() && dst < end {
                if src[idx] == b'$' && idx + 1 < src.len() && src[idx + 1].is_ascii_digit() {
                    let mut param_num = 0;
                    let mut p = idx + 1;
                    while p < src.len() && src[p].is_ascii_digit() {
                        param_num = param_num * 10 + (src[p] - b'0') as usize;
                        p += 1;
                    }
                    let param_idx = param_num.saturating_sub(1);
                    if param_idx < (*pg_stmt).param_count as usize && param_idx < MAX_PARAMS {
                        let val = (*pg_stmt).param_values[param_idx];
                        if !val.is_null() {
                            if dst < end {
                                *dst = b'\'';
                                dst = dst.add(1);
                            }
                            let bytes = CStr::from_ptr(val).to_bytes();
                            for &b in bytes {
                                if dst >= end {
                                    break;
                                }
                                if b == b'\'' && dst < end {
                                    *dst = b'\'';
                                    dst = dst.add(1);
                                    if dst >= end {
                                        break;
                                    }
                                }
                                *dst = b;
                                dst = dst.add(1);
                            }
                            if dst < end {
                                *dst = b'\'';
                                dst = dst.add(1);
                            }
                        } else if (dst as usize) + 4 < end as usize {
                            std::ptr::copy_nonoverlapping(b"NULL".as_ptr(), dst, 4);
                            dst = dst.add(4);
                        }
                    } else {
                        for &b in &src[idx..p] {
                            if dst >= end {
                                break;
                            }
                            *dst = b;
                            dst = dst.add(1);
                        }
                    }
                    idx = p;
                } else {
                    *dst = src[idx];
                    dst = dst.add(1);
                    idx += 1;
                }
            }
            if dst <= end {
                *dst = 0;
            }
            return result;
        }
    }

    unsafe {
        if let Some(f) = orig_sqlite3_expanded_sql {
            return f(p_stmt);
        }
    }
    std::ptr::null_mut()
}
