use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

use crate::db_interpose_common::tls_in_interpose_call_ptr;
use crate::db_interpose_conn_utils::{cstr_to_string_or, log_debug, PthreadMutexGuard};
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgStmt, MAX_PARAMS};
const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

const PGRES_TUPLES_OK: c_int = 2;
const PGRES_FATAL_ERROR: c_int = 7;

static NOT_AN_ERROR: &[u8] = b"not an error\0";

type CollationCompare =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int>;
type CollationDestroy = Option<unsafe extern "C" fn(*mut c_void)>;

#[repr(C)]
struct SqlTranslation {
    sql: *mut c_char,
    param_names: *mut *mut c_char,
    param_count: c_int,
    success: c_int,
    error: [c_char; 256],
}

extern "C" {
    static mut orig_sqlite3_get_table: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_char,
            *mut *mut *mut c_char,
            *mut c_int,
            *mut c_int,
            *mut *mut c_char,
        ) -> c_int,
    >;

    static mut orig_sqlite3_errmsg: Option<unsafe extern "C" fn(*mut sqlite3) -> *const c_char>;
    static mut orig_sqlite3_errcode: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int>;
    static mut orig_sqlite3_extended_errcode: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int>;

    static mut orig_sqlite3_create_collation: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_char,
            c_int,
            *mut c_void,
            CollationCompare,
        ) -> c_int,
    >;
    static mut orig_sqlite3_create_collation_v2: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_char,
            c_int,
            *mut c_void,
            CollationCompare,
            CollationDestroy,
        ) -> c_int,
    >;

    static mut orig_sqlite3_free: Option<unsafe extern "C" fn(*mut c_void)>;
    static mut orig_sqlite3_malloc: Option<unsafe extern "C" fn(c_int) -> *mut c_void>;

    static mut orig_sqlite3_db_handle: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut sqlite3>;
    static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char>;
    static mut orig_sqlite3_bind_parameter_count: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_stmt_readonly: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_stmt_busy: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_stmt_status: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, c_int) -> c_int>;
    static mut orig_sqlite3_bind_parameter_name: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_char,
    >;
    static mut orig_sqlite3_bind_parameter_index: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, *const c_char) -> c_int,
    >;
    static mut orig_sqlite3_expanded_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut c_char>;

    static mut shim_sqlite3_errmsg: Option<unsafe extern "C" fn(*mut sqlite3) -> *const c_char>;
    static mut shim_sqlite3_errcode: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int>;

    fn sql_translate(sql: *const c_char) -> SqlTranslation;
    fn sql_translation_free(result: *mut SqlTranslation);
}

struct InterposeGuard;

impl InterposeGuard {
    fn try_enter() -> Option<Self> {
        unsafe {
            let flag = tls_in_interpose_call_ptr();
            if *flag != 0 {
                return None;
            }
            *flag = 1;
            Some(InterposeGuard)
        }
    }
}

impl Drop for InterposeGuard {
    fn drop(&mut self) {
        unsafe {
            *tls_in_interpose_call_ptr() = 0;
        }
    }
}

fn contains_icase_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_changes(db: *mut sqlite3) -> c_int {
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    let mut result = 0;
    unsafe {
        if !pg_conn.is_null() && (*pg_conn).is_pg_active != 0 {
            result = (*pg_conn).last_changes;
        }
    }
    result
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_changes64(db: *mut sqlite3) -> i64 {
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    let mut result: i64 = 0;
    unsafe {
        if !pg_conn.is_null() && (*pg_conn).is_pg_active != 0 {
            result = (*pg_conn).last_changes as i64;
        }
    }
    result
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64 {
    if unsafe { *tls_in_interpose_call_ptr() } != 0 {
        log_debug("last_insert_rowid: RECURSION DETECTED, returning 0");
        return 0;
    }
    let _guard = match InterposeGuard::try_enter() {
        Some(g) => g,
        None => return 0,
    };

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if pg_conn.is_null() {
        let global_rowid = crate::pg_client::rust_get_global_last_insert_rowid();
        log_debug(&format!(
            "last_insert_rowid: CALLED db={:p} pg_conn=NULL (no exact match, global={})",
            db, global_rowid
        ));
        return if global_rowid > 0 { global_rowid } else { 0 };
    }

    log_debug(&format!(
        "last_insert_rowid: CALLED db={:p} pg_conn={:p} (exact match)",
        db, pg_conn
    ));

    unsafe {
        if (*pg_conn).last_insert_rowid > 0 {
            let rowid = (*pg_conn).last_insert_rowid;
            log_debug(&format!(
                "last_insert_rowid: using cached connection rowid={}",
                rowid
            ));
            return rowid;
        }
    }

    let global_rowid = crate::pg_client::rust_get_global_last_insert_rowid();
    if global_rowid > 0 {
        log_debug(&format!(
            "last_insert_rowid: using cached global rowid={}",
            global_rowid
        ));
        return global_rowid;
    }

    let mut result: i64 = 0;
    unsafe {
        if !pg_conn.is_null()
            && (*pg_conn).is_pg_active != 0
            && !(*pg_conn).conn.is_null()
        {
            let mut conn_guard = PthreadMutexGuard::lock(&mut (*pg_conn).mutex as *mut _);
            log_debug(&format!(
                "last_insert_rowid: EXECUTING lastval() on conn {:p}",
                (*pg_conn).conn
            ));
            let res = crate::libpq_helpers::rust_pq_exec(
                (*pg_conn).conn,
                b"SELECT lastval()\0".as_ptr() as *const c_char,
            );
            if res.is_null() {
                conn_guard.unlock();
                log_debug("last_insert_rowid: NULL result, RETURNING 0");
                return 0;
            }

            let status = crate::libpq_helpers::rust_pq_result_status(res);
            log_debug(&format!(
                "last_insert_rowid: STATUS={} TUPLES={}",
                status,
                crate::libpq_helpers::rust_pq_ntuples(res)
            ));
            if status == PGRES_TUPLES_OK && crate::libpq_helpers::rust_pq_ntuples(res) > 0 {
                let mut val_buf = [0 as c_char; 64];
                let mut val_str: *const c_char = b"0\0".as_ptr() as *const c_char;
                if crate::db_interpose_helpers::rust_pg_result_text_copy(
                    res as *const crate::db_interpose_helpers::PGresult,
                    0,
                    0,
                    val_buf.as_mut_ptr(),
                    val_buf.len(),
                ) >= 0
                {
                    val_str = val_buf.as_ptr();
                }
                let rowid = crate::db_interpose_helpers::rust_pg_text_to_int64(val_str);
                log_debug(&format!(
                    "last_insert_rowid: GOT VALUE={} rowid={}",
                    cstr_to_string_or(val_str, "0"),
                    rowid
                ));
                crate::libpq_helpers::rust_pq_clear(res);
                conn_guard.unlock();
                if rowid > 0 {
                    log_debug(&format!("last_insert_rowid: RETURNING rowid={}", rowid));
                    result = rowid;
                } else {
                    log_debug("last_insert_rowid: rowid <= 0, RETURNING 0");
                }
            } else {
                if status == PGRES_FATAL_ERROR {
                    let err = crate::libpq_helpers::rust_pq_error_message((*pg_conn).conn);
                    log_debug(&format!(
                        "last_insert_rowid: FATAL_ERROR: {}",
                        cstr_to_string_or(err, "(null)")
                    ));
                } else {
                    log_debug(&format!("last_insert_rowid: NON-TUPLES status={}", status));
                }
                crate::libpq_helpers::rust_pq_clear(res);
                conn_guard.unlock();
                log_debug("last_insert_rowid: RETURNING 0 due to error");
            }
        } else {
            log_debug("last_insert_rowid: NO PG_CONN or not active, RETURNING 0");
        }
    }

    log_debug(&format!("last_insert_rowid: FINAL result={}", result));
    result
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_errmsg(db: *mut sqlite3) -> *const c_char {
    log_debug(&format!("ERRMSG: db={:p}", db));
    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            if let Some(f) = shim_sqlite3_errmsg {
                return f(db);
            }
        }
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn.is_null() {
        unsafe {
            if (*pg_conn).last_error_code != SQLITE_OK && (*pg_conn).last_error[0] != 0 {
                log_debug(&format!(
                    "ERRMSG: returning tracked error='{}'",
                    cstr_to_string_or((*pg_conn).last_error.as_ptr(), "")
                ));
                return (*pg_conn).last_error.as_ptr();
            }
        }
        log_debug("ERRMSG: returning 'not an error'");
        return NOT_AN_ERROR.as_ptr() as *const c_char;
    }

    unsafe {
        if let Some(f) = shim_sqlite3_errmsg {
            return f(db);
        }
        if let Some(f) = orig_sqlite3_errmsg {
            return f(db);
        }
    }
    b"unknown error\0".as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_errcode(db: *mut sqlite3) -> c_int {
    log_debug(&format!("ERRCODE: db={:p}", db));
    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            if let Some(f) = shim_sqlite3_errcode {
                return f(db);
            }
        }
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn.is_null() {
        unsafe {
            log_debug(&format!(
                "ERRCODE: pg_conn found, returning code={}",
                (*pg_conn).last_error_code
            ));
            return (*pg_conn).last_error_code;
        }
    }

    unsafe {
        if let Some(f) = shim_sqlite3_errcode {
            return f(db);
        }
        if let Some(f) = orig_sqlite3_errcode {
            return f(db);
        }
    }
    SQLITE_ERROR
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_extended_errcode(db: *mut sqlite3) -> c_int {
    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn.is_null() {
        unsafe {
            return (*pg_conn).last_error_code;
        }
    }
    unsafe {
        if let Some(f) = orig_sqlite3_extended_errcode {
            return f(db);
        }
    }
    SQLITE_ERROR
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_get_table(
    db: *mut sqlite3,
    sql: *const c_char,
    paz_result: *mut *mut *mut c_char,
    pn_row: *mut c_int,
    pn_column: *mut c_int,
    pz_err_msg: *mut *mut c_char,
) -> c_int {
    if sql.is_null() {
        unsafe {
            return match orig_sqlite3_get_table {
                Some(f) => f(db, sql, paz_result, pn_row, pn_column, pz_err_msg),
                None => SQLITE_ERROR,
            };
        }
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);
    unsafe {
        if !pg_conn.is_null()
            && (*pg_conn).is_pg_active != 0
            && !(*pg_conn).conn.is_null()
            && crate::pg_config::pg_config_is_read_operation(sql) != 0
        {
            let mut trans = sql_translate(sql);
            if trans.success != 0 && !trans.sql.is_null() {
                let mut conn_guard = PthreadMutexGuard::lock(&mut (*pg_conn).mutex as *mut _);
                let res = crate::libpq_helpers::rust_pq_exec((*pg_conn).conn, trans.sql);
                if crate::libpq_helpers::rust_pq_result_status(res) == PGRES_TUPLES_OK {
                    let mut result: *mut *mut c_char = std::ptr::null_mut();
                    let mut nrows = 0;
                    let mut ncols = 0;
                    if crate::db_interpose_helpers::rust_get_table_from_pgresult(
                        res as *const crate::db_interpose_helpers::PGresult,
                        &mut result,
                        &mut nrows,
                        &mut ncols,
                    ) != 0
                    {
                        if !paz_result.is_null() {
                            *paz_result = result;
                        }
                        if !pn_row.is_null() {
                            *pn_row = nrows;
                        }
                        if !pn_column.is_null() {
                            *pn_column = ncols;
                        }
                        if !pz_err_msg.is_null() {
                            *pz_err_msg = std::ptr::null_mut();
                        }
                        crate::libpq_helpers::rust_pq_clear(res);
                        conn_guard.unlock();
                        sql_translation_free(&mut trans as *mut SqlTranslation);
                        return SQLITE_OK;
                    }
                }
                crate::libpq_helpers::rust_pq_clear(res);
                conn_guard.unlock();
            }
            sql_translation_free(&mut trans as *mut SqlTranslation);
        }
    }

    unsafe {
        match orig_sqlite3_get_table {
            Some(f) => f(db, sql, paz_result, pn_row, pn_column, pz_err_msg),
            None => SQLITE_ERROR,
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_create_collation(
    db: *mut sqlite3,
    name: *const c_char,
    text_rep: c_int,
    arg: *mut c_void,
    compare: CollationCompare,
) -> c_int {
    if !name.is_null() {
        let name_bytes = unsafe { CStr::from_ptr(name).to_bytes() };
        if contains_icase_bytes(name_bytes, b"icu") {
            log_debug(&format!(
                "Faking registration of collation: {}",
                cstr_to_string_or(name, "")
            ));
            return SQLITE_OK;
        }
    }
    unsafe {
        match orig_sqlite3_create_collation {
            Some(f) => f(db, name, text_rep, arg, compare),
            None => SQLITE_ERROR,
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_create_collation_v2(
    db: *mut sqlite3,
    name: *const c_char,
    text_rep: c_int,
    arg: *mut c_void,
    compare: CollationCompare,
    destroy: CollationDestroy,
) -> c_int {
    if !name.is_null() {
        let name_bytes = unsafe { CStr::from_ptr(name).to_bytes() };
        if contains_icase_bytes(name_bytes, b"icu") {
            log_debug(&format!(
                "Faking registration of collation v2: {}",
                cstr_to_string_or(name, "")
            ));
            return SQLITE_OK;
        }
    }
    unsafe {
        match orig_sqlite3_create_collation_v2 {
            Some(f) => f(db, name, text_rep, arg, compare, destroy),
            None => SQLITE_ERROR,
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_free(ptr: *mut c_void) {
    unsafe {
        if let Some(f) = orig_sqlite3_free {
            f(ptr);
        } else {
            libc::free(ptr);
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_malloc(n: c_int) -> *mut c_void {
    unsafe {
        if let Some(f) = orig_sqlite3_malloc {
            return f(n);
        }
    }
    unsafe { libc::malloc(n as usize) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_db_handle(p_stmt: *mut sqlite3_stmt) -> *mut sqlite3 {
    log_debug(&format!("DB_HANDLE: pStmt={:p}", p_stmt));
    if p_stmt.is_null() {
        return std::ptr::null_mut();
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
        unsafe {
            if !(*pg_stmt).shadow_stmt.is_null() {
                if let Some(f) = orig_sqlite3_db_handle {
                    let db = f((*pg_stmt).shadow_stmt);
                    log_debug(&format!("DB_HANDLE: returning from shadow_stmt={:p}", db));
                    return db;
                }
            }
            if !(*pg_stmt).conn.is_null() && !(*(*pg_stmt).conn).shadow_db.is_null() {
                log_debug(&format!(
                    "DB_HANDLE: returning shadow_db={:p}",
                    (*(*pg_stmt).conn).shadow_db
                ));
                return (*(*pg_stmt).conn).shadow_db;
            }
        }
        log_debug("DB_HANDLE: pg_stmt has no valid db handle");
        return std::ptr::null_mut();
    }

    unsafe {
        if let Some(f) = orig_sqlite3_db_handle {
            let db = f(p_stmt);
            log_debug(&format!("DB_HANDLE: returning orig={:p}", db));
            return db;
        }
    }
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_sql(p_stmt: *mut sqlite3_stmt) -> *const c_char {
    if p_stmt.is_null() {
        return std::ptr::null();
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
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

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_parameter_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    if p_stmt.is_null() {
        return 0;
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
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

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_stmt_readonly(p_stmt: *mut sqlite3_stmt) -> c_int {
    if p_stmt.is_null() {
        return 1;
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
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

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_stmt_busy(p_stmt: *mut sqlite3_stmt) -> c_int {
    log_debug(&format!("STMT_BUSY: stmt={:p}", p_stmt));
    if p_stmt.is_null() {
        return 0;
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
        unsafe {
            let busy = !(*pg_stmt).result.is_null() && (*pg_stmt).current_row < (*pg_stmt).num_rows;
            log_debug(&format!(
                "STMT_BUSY: pg_stmt, result={:p} current_row={} num_rows={} -> busy={}",
                (*pg_stmt).result, (*pg_stmt).current_row, (*pg_stmt).num_rows, busy as i32
            ));
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

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_stmt_status(p_stmt: *mut sqlite3_stmt, op: c_int, reset: c_int) -> c_int {
    log_debug(&format!("STMT_STATUS: stmt={:p} op={} reset={}", p_stmt, op, reset));
    if p_stmt.is_null() {
        return 0;
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
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

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_parameter_name(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    log_debug(&format!("BIND_PARAM_NAME: stmt={:p} idx={}", p_stmt, idx));
    if p_stmt.is_null() {
        return std::ptr::null();
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
        unsafe {
            if idx > 0 && idx <= (*pg_stmt).param_count && !(*pg_stmt).param_names.is_null() {
                let name = *(*pg_stmt).param_names.add((idx - 1) as usize);
                log_debug(&format!(
                    "BIND_PARAM_NAME: pg_stmt returning '{}'",
                    cstr_to_string_or(name, "NULL")
                ));
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

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_parameter_index(
    p_stmt: *mut sqlite3_stmt,
    name: *const c_char,
) -> c_int {
    if p_stmt.is_null() || name.is_null() {
        return 0;
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
        unsafe {
            if (*pg_stmt).param_names.is_null() || (*pg_stmt).param_count == 0 {
                log_debug(&format!(
                    "BIND_PARAM_INDEX: pg_stmt has no params, falling through to SQLite for '{}'",
                    cstr_to_string_or(name, "")
                ));
            } else {
                let mut name_to_find = name;
                let first = *name as u8;
                if first == b':' || first == b'@' || first == b'$' {
                    name_to_find = name.add(1);
                }
                for i in 0..(*pg_stmt).param_count {
                    let cur = *(*pg_stmt).param_names.add(i as usize);
                    if !cur.is_null()
                        && !name_to_find.is_null()
                        && libc::strcmp(cur, name_to_find) == 0
                    {
                        log_debug(&format!(
                            "BIND_PARAM_INDEX: found '{}' at index {}",
                            cstr_to_string_or(name, ""),
                            i + 1
                        ));
                        return i + 1;
                    }
                }
                log_debug(&format!(
                    "BIND_PARAM_INDEX: '{}' not found in pg_stmt (param_count={})",
                    cstr_to_string_or(name, ""),
                    (*pg_stmt).param_count
                ));
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

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_expanded_sql(p_stmt: *mut sqlite3_stmt) -> *mut c_char {
    if p_stmt.is_null() {
        return std::ptr::null_mut();
    }

    let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } == 2 {
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
                let result = rust_my_sqlite3_malloc((base_len + 1) as c_int) as *mut c_char;
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

            let result = rust_my_sqlite3_malloc(estimated as c_int) as *mut c_char;
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
                    if param_idx < (*pg_stmt).param_count as usize
                        && param_idx < MAX_PARAMS
                    {
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
