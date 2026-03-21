use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::Ordering;

use crate::db_interpose_common::tls_in_resolve_tables_ptr;
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt, MAX_PARAMS};

const SQLITE_DONE: c_int = 101;
const SQLITE_ROW: c_int = 100;
const SQLITE_ERROR: c_int = 1;

const STEP_RESULT_FALLBACK: c_int = -1;
const STEP_RESULT_DONE: c_int = SQLITE_DONE;
const STEP_RESULT_ROW: c_int = SQLITE_ROW;
const STEP_RESULT_ERROR: c_int = SQLITE_ERROR;

const PG_RETRY_MAX_DELAYS: usize = 10;
const PQTRANS_IDLE: c_int = 0;

thread_local! {
    static STEP_PG_CONN_ERROR: Cell<i32> = Cell::new(0);
    static STEP_RETRY_COUNT: Cell<i32> = Cell::new(0);
}

#[repr(C)]
struct SqlTranslation {
    sql: *mut c_char,
    param_names: *mut *mut c_char,
    param_count: c_int,
    success: c_int,
    error: [c_char; 256],
}

extern "C" {
    static mut orig_sqlite3_step: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char>;
    static mut orig_sqlite3_db_handle: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut sqlite3>;
    static mut orig_sqlite3_expanded_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut c_char>;
    static mut orig_sqlite3_free: Option<unsafe extern "C" fn(*mut c_void)>;

    fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3;
    fn sqlite3_sql(stmt: *mut sqlite3_stmt) -> *const c_char;
    fn sqlite3_expanded_sql(stmt: *mut sqlite3_stmt) -> *mut c_char;
    fn sqlite3_free(ptr: *mut c_void);

    fn shim_alloc_maybe_log();
    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        p_stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );

    fn sql_translate(sql: *const c_char) -> SqlTranslation;
    fn sql_translation_free(result: *mut SqlTranslation);
}

fn log_error(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(0, cs.as_ptr());
    }
}

fn log_debug(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(2, cs.as_ptr());
    }
}

fn cstr_prefix(ptr: *const c_char, max_len: usize, default: &str) -> String {
    if ptr.is_null() {
        return default.to_string();
    }
    let bytes = unsafe { CStr::from_ptr(ptr).to_bytes() };
    let slice = &bytes[..bytes.len().min(max_len)];
    String::from_utf8_lossy(slice).into_owned()
}

unsafe fn orig_step(p_stmt: *mut sqlite3_stmt) -> c_int {
    match orig_sqlite3_step {
        Some(f) => f(p_stmt),
        None => SQLITE_ERROR,
    }
}

unsafe fn call_sqlite3_sql(p_stmt: *mut sqlite3_stmt) -> *const c_char {
    match orig_sqlite3_sql {
        Some(f) => f(p_stmt),
        None => sqlite3_sql(p_stmt),
    }
}

unsafe fn call_sqlite3_expanded_sql(p_stmt: *mut sqlite3_stmt) -> *mut c_char {
    match orig_sqlite3_expanded_sql {
        Some(f) => f(p_stmt),
        None => sqlite3_expanded_sql(p_stmt),
    }
}

unsafe fn call_sqlite3_free(ptr: *mut c_void) {
    if let Some(f) = orig_sqlite3_free {
        f(ptr);
    } else {
        sqlite3_free(ptr);
    }
}

unsafe fn step_handle_cached_stmt(p_stmt: *mut sqlite3_stmt) -> c_int {
    let db = sqlite3_db_handle(p_stmt);
    let mut pg_conn = crate::pg_client::rust_pg_find_connection(db);
    if pg_conn.is_null() {
        pg_conn = crate::pg_client::rust_pg_find_any_library_connection();
    }

    if pg_conn.is_null()
        || (*pg_conn).is_pg_active == 0
        || (*pg_conn).conn.is_null()
        || crate::db_interpose_helpers::rust_is_library_or_blobs_db_path((*pg_conn).db_path.as_ptr())
            == 0
    {
        return STEP_RESULT_FALLBACK;
    }

    let expanded_sql = call_sqlite3_expanded_sql(p_stmt);
    let sql = if !expanded_sql.is_null() {
        expanded_sql as *const c_char
    } else {
        call_sqlite3_sql(p_stmt)
    };

    let orig_sql = call_sqlite3_sql(p_stmt);
    if !sql.is_null()
        && crate::pg_config::pg_config_is_write_operation(sql) != 0
        && crate::pg_config::pg_config_should_skip_sql(sql) == 0
        && crate::pg_config::pg_config_should_skip_sql(orig_sql) == 0
    {
        if !sql.is_null()
            && CStr::from_ptr(sql)
                .to_bytes()
                .windows(b"INSERT".len())
                .any(|w| w.eq_ignore_ascii_case(b"INSERT"))
            && CStr::from_ptr(sql)
                .to_bytes()
                .windows(b"metadata_items".len())
                .any(|w| w.eq_ignore_ascii_case(b"metadata_items"))
        {
            log_debug("CACHED INSERT metadata_items:");
            log_debug(&format!(
                "  expanded_sql={}",
                if expanded_sql.is_null() { "NO" } else { "YES" }
            ));
            log_debug(&format!(
                "  sql (first 300): {}",
                cstr_prefix(sql, 300, "(null)")
            ));
        }
        if crate::db_interpose_helpers::rust_is_junk_metadata_insert(sql) != 0 {
            log_error(
                "GUARD: Blocked cached junk INSERT into metadata_items (library_section_id=NULL, metadata_type=NULL)",
            );
            if !expanded_sql.is_null() {
                call_sqlite3_free(expanded_sql as *mut c_void);
            }
            return STEP_RESULT_DONE;
        }

        let mut cached =
            crate::pg_statement::rust_cached_stmt_find(p_stmt as usize) as *mut PgStmt;
        if !cached.is_null() && (*cached).write_executed != 0 {
            if !expanded_sql.is_null() {
                call_sqlite3_free(expanded_sql as *mut c_void);
            }
            return STEP_RESULT_DONE;
        }

        let mut cached_exec_conn: *mut PgConnection = std::ptr::null_mut();
        if crate::db_interpose_step_write_utils::rust_step_cached_write_should_noop(
            pg_conn,
            sql,
            &mut cached_exec_conn,
        ) != 0
        {
            if !expanded_sql.is_null() {
                call_sqlite3_free(expanded_sql as *mut c_void);
            }
            return STEP_RESULT_DONE;
        }

        let mut trans = sql_translate(sql);
        if trans.success != 0 && !trans.sql.is_null() {
            let mut exec_sql = trans.sql as *const c_char;
            let insert_sql = crate::db_interpose_step_write_utils::rust_step_cached_write_build_exec_sql(
                sql,
                trans.sql,
                &mut exec_sql,
            );
            let mut cached_write_conn_error = 0;
            let cached_write_rc =
                crate::db_interpose_step_write_utils::rust_step_cached_write_execute_and_finalize(
                    &mut cached,
                    p_stmt,
                    pg_conn,
                    cached_exec_conn,
                    sql,
                    exec_sql,
                    &mut cached_write_conn_error,
                );
            if !insert_sql.is_null() {
                libc::free(insert_sql as *mut c_void);
            }
            if cached_write_rc == STEP_RESULT_ERROR {
                sql_translation_free(&mut trans as *mut SqlTranslation);
                if !expanded_sql.is_null() {
                    call_sqlite3_free(expanded_sql as *mut c_void);
                }
                if cached_write_conn_error != 0 {
                    STEP_PG_CONN_ERROR.with(|c| c.set(1));
                }
                return STEP_RESULT_ERROR;
            }
        }
        sql_translation_free(&mut trans as *mut SqlTranslation);
        if !expanded_sql.is_null() {
            call_sqlite3_free(expanded_sql as *mut c_void);
        }
        return STEP_RESULT_DONE;
    }

    if !sql.is_null()
        && crate::pg_config::pg_config_is_read_operation(sql) != 0
        && crate::pg_config::pg_config_should_skip_sql(sql) == 0
    {
        let cached_read_conn =
            crate::db_interpose_step_write_utils::rust_step_pick_thread_connection(pg_conn);
        let mut cached_branch_rc = STEP_RESULT_FALLBACK;
        let cached = crate::pg_statement::rust_cached_stmt_find(p_stmt as usize) as *mut PgStmt;
        let sqlite_result = orig_step(p_stmt);

        if sqlite_result == SQLITE_ROW || sqlite_result == SQLITE_DONE {
            let mut cached_rc = STEP_RESULT_DONE;
            if crate::db_interpose_step_cached_read_utils::rust_step_cached_read_finalize_advance(
                cached,
                expanded_sql,
                &mut cached_rc,
            ) != 0
            {
                return cached_rc;
            }

            let mut trans = sql_translate(sql);
            if trans.success != 0 && !trans.sql.is_null() {
                let new_stmt =
                    crate::db_interpose_step_cached_read_utils::rust_step_cached_read_prepare_stmt(
                        cached,
                        cached_read_conn,
                        sql,
                        p_stmt,
                        trans.sql,
                    );
                if !new_stmt.is_null() {
                    let mut conn_error = 0;
                    cached_branch_rc =
                        crate::db_interpose_step_cached_read_utils::rust_step_cached_read_execute(
                            new_stmt,
                            cached_read_conn,
                            sql,
                            trans.sql,
                            &mut conn_error,
                        );
                    if conn_error != 0 && cached_branch_rc == STEP_RESULT_ERROR {
                        STEP_PG_CONN_ERROR.with(|c| c.set(1));
                    }
                }
            }
            sql_translation_free(&mut trans as *mut SqlTranslation);
        }

        if cached_branch_rc == STEP_RESULT_ROW
            || cached_branch_rc == STEP_RESULT_DONE
            || cached_branch_rc == STEP_RESULT_ERROR
        {
            if !expanded_sql.is_null() {
                call_sqlite3_free(expanded_sql as *mut c_void);
            }
            return cached_branch_rc;
        }

        if !expanded_sql.is_null() {
            call_sqlite3_free(expanded_sql as *mut c_void);
        }
        return sqlite_result;
    }

    if !expanded_sql.is_null() {
        call_sqlite3_free(expanded_sql as *mut c_void);
    }
    STEP_RESULT_FALLBACK
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_step(p_stmt: *mut sqlite3_stmt) -> c_int {
    let dbg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
    let mut dbg_sql: *const c_char = std::ptr::null();
    let mut dbg_db: *mut sqlite3 = std::ptr::null_mut();

    unsafe {
        if !dbg_stmt.is_null() {
            dbg_sql = if !(*dbg_stmt).pg_sql.is_null() {
                (*dbg_stmt).pg_sql
            } else {
                (*dbg_stmt).sql
            };
        }
        if dbg_sql.is_null() {
            if let Some(f) = orig_sqlite3_sql {
                dbg_sql = f(p_stmt);
            }
        }
        if let Some(f) = orig_sqlite3_db_handle {
            dbg_db = f(p_stmt);
        }
    }

    let phase = b"step\0";
    unsafe {
        pg_exception_note_phase(phase.as_ptr() as *const c_char, dbg_sql, p_stmt, dbg_db);
    }

    let rc = unsafe { my_sqlite3_step_impl(p_stmt) };

    let mut delays = [0i32; PG_RETRY_MAX_DELAYS];
    let mut max_retries = 0i32;
    crate::pg_config::pg_config_get_retry_delays(delays.as_mut_ptr(), &mut max_retries);

    let retry_count = STEP_RETRY_COUNT.with(|c| c.get());
    if rc == SQLITE_ERROR && retry_count < max_retries {
        let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
        let conn_error = STEP_PG_CONN_ERROR.with(|c| c.get());
        if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg } != 0 && conn_error != 0 {
            STEP_PG_CONN_ERROR.with(|c| c.set(0));
            let delay = delays[retry_count as usize];
            let new_count = retry_count + 1;
            STEP_RETRY_COUNT.with(|c| c.set(new_count));
            log_error(&format!(
                "step: PG conn error, retry {}/{} in {}ms (thread {:p})",
                new_count,
                max_retries,
                delay,
                unsafe { libc::pthread_self() } as *mut c_void
            ));

            unsafe {
                libc::pthread_mutex_lock(&mut (*pg_stmt).mutex as *mut _);
                crate::pg_statement::rust_stmt_clear_result(pg_stmt);
                libc::pthread_mutex_unlock(&mut (*pg_stmt).mutex as *mut _);
            }

            let delay_ms = if delay < 0 { 0 } else { delay as u32 };
            unsafe {
                libc::usleep(delay_ms.saturating_mul(1000));
            }
            STEP_PG_CONN_ERROR.with(|c| c.set(0));
            let retry_rc = rust_my_sqlite3_step(p_stmt);

            if new_count > 0 && retry_rc != SQLITE_ERROR {
                log_error(&format!(
                    "step: retry succeeded after {} attempt(s)",
                    new_count
                ));
            }
            STEP_RETRY_COUNT.with(|c| c.set(0));
            return retry_rc;
        }
    }

    if retry_count > 0 {
        if rc == SQLITE_ERROR {
            log_error("step: retries exhausted, returning SQLITE_ERROR");
        }
        STEP_RETRY_COUNT.with(|c| c.set(0));
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
        log_debug(&format!(
            "[RACE_DEBUG] STEP_END thread={:p} stmt={:p} rc={} reason=skip",
            libc::pthread_self() as *mut c_void,
            p_stmt,
            SQLITE_DONE
        ));
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
        let db = sqlite3_db_handle((*pg_stmt).shadow_stmt);
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
                let thread_conn = crate::pg_client::rust_pool_get_connection(
                    (*handle_conn).db_path.as_ptr(),
                ) as *mut PgConnection;
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
        libc::pthread_mutex_lock(&mut (*pg_stmt).mutex as *mut _);

        let mut param_values: [*const c_char; MAX_PARAMS] = [std::ptr::null(); MAX_PARAMS];
        let max_params = (*pg_stmt).param_count.min(MAX_PARAMS as c_int);
        for i in 0..max_params {
            param_values[i as usize] = (*pg_stmt).param_values[i as usize] as *const c_char;
        }

        if (*pg_stmt).is_pg == 2 {
            if (*pg_stmt).read_done != 0 {
                libc::pthread_mutex_unlock(&mut (*pg_stmt).mutex as *mut _);
                return SQLITE_DONE;
            }

            if !(*pg_stmt).cached_result.is_null() {
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

            if (*pg_stmt).streaming_mode != 0 {
                return crate::db_interpose_step_read_utils::rust_step_read_streaming_next(
                    p_stmt, pg_stmt,
                );
            }

            if !(*pg_stmt).result.is_null() {
                return crate::db_interpose_step_read_utils::rust_step_read_eager_next(pg_stmt);
            }

            if (*pg_stmt).result.is_null() {
                let mut conn_error = 0;
                let first_rc = crate::db_interpose_step_read_utils::rust_step_read_first_execute(
                    pg_stmt,
                    &mut exec_conn,
                    param_values.as_ptr(),
                    &mut conn_error,
                );
                if first_rc == STEP_RESULT_ERROR && conn_error != 0 {
                    STEP_PG_CONN_ERROR.with(|c| c.set(1));
                }
                return first_rc;
            }
        } else if (*pg_stmt).is_pg == 1 {
            if (*pg_stmt).write_executed != 0 {
                libc::pthread_mutex_unlock(&mut (*pg_stmt).mutex as *mut _);
                return SQLITE_DONE;
            }

            let mut txn_state = PQTRANS_IDLE;
            if crate::db_interpose_step_write_utils::rust_step_pg_write_should_noop(
                exec_conn,
                (*pg_stmt).pg_sql,
                &mut txn_state,
            ) != 0
            {
                log_debug(&format!(
                    "TXN_NOOP: skipping tx terminator in state={} sql={}",
                    txn_state,
                    cstr_prefix((*pg_stmt).pg_sql, 120, "(null)")
                ));
                (*pg_stmt).write_executed = 1;
                libc::pthread_mutex_unlock(&mut (*pg_stmt).mutex as *mut _);
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
                libc::pthread_mutex_unlock(&mut (*pg_stmt).mutex as *mut _);
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
                    STEP_PG_CONN_ERROR.with(|c| c.set(1));
                }
                return SQLITE_ERROR;
            }

            let mut write_conn_error = 0;
            let write_rc = crate::db_interpose_step_write_utils::rust_step_write_execute_and_finalize(
                pg_stmt,
                exec_conn,
                param_values.as_ptr(),
                &mut write_conn_error,
            );
            if write_rc == STEP_RESULT_ERROR {
                libc::pthread_mutex_unlock(&mut (*pg_stmt).mutex as *mut _);
                if write_conn_error != 0 {
                    STEP_PG_CONN_ERROR.with(|c| c.set(1));
                }
                return SQLITE_ERROR;
            }
        }

        libc::pthread_mutex_unlock(&mut (*pg_stmt).mutex as *mut _);
    }

    if !pg_stmt.is_null() && (*pg_stmt).is_pg != 0 {
        if (*pg_stmt).is_pg == 1 {
            return SQLITE_DONE;
        }
        crate::db_interpose_step_write_utils::rust_step_log_step_exit_trace(pg_stmt);
    }

    orig_step(p_stmt)
}
