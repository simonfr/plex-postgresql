use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::db_interpose_conn_utils::{
    apply_pg_session_settings, connect_new, log_debug, log_error, log_info, PthreadMutexGuard,
    PgConnConfig, STATEMENT_TIMEOUT_SQL,
};
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt, MAX_PARAMS};
use crate::libpq_helpers::PGresult;

const SQLITE_DONE: c_int = 101;
const SQLITE_ROW: c_int = 100;
const SQLITE_ERROR: c_int = 1;
const STEP_RESULT_DONE: c_int = SQLITE_DONE;
const STEP_RESULT_ROW: c_int = SQLITE_ROW;
const STEP_RESULT_ERROR: c_int = SQLITE_ERROR;

const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const PGRES_SINGLE_TUPLE: c_int = 9;
const CONNECTION_OK: c_int = 0;
const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

static TRACE_PLAY_QUEUE: AtomicI32 = AtomicI32::new(-1);

extern "C" {
    fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3;
    fn resolve_column_tables(pg_stmt: *mut PgStmt, pg_conn: *mut PgConnection) -> c_int;
    fn log_sql_fallback(
        original_sql: *const c_char,
        translated_sql: *const c_char,
        error_msg: *const c_char,
        context: *const c_char,
    );
    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);
    fn pg_exception_note_query(sql: *const c_char);
    fn pg_config_get() -> *mut PgConnConfig;
}

fn ascii_lower(b: u8) -> u8 {
    if (b'A'..=b'Z').contains(&b) {
        b + 32
    } else {
        b
    }
}

fn contains_icase_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| {
        w.iter()
            .zip(needle.iter())
            .all(|(a, b)| ascii_lower(*a) == ascii_lower(*b))
    })
}

fn cstr_to_str(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "?";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("?") }
}

fn cstr_bytes(ptr: *const c_char) -> &'static [u8] {
    if ptr.is_null() {
        return &[];
    }
    unsafe { CStr::from_ptr(ptr).to_bytes() }
}

fn trace_play_queue_enabled() -> bool {
    let cached = TRACE_PLAY_QUEUE.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let key = CString::new("PLEX_PG_TRACE_PLAY_QUEUE").unwrap();
    let val = unsafe { libc::getenv(key.as_ptr()) };
    let enabled = crate::db_interpose_helpers::rust_env_truthy(val) != 0;
    TRACE_PLAY_QUEUE.store(if enabled { 1 } else { 0 }, Ordering::Relaxed);
    enabled
}

unsafe fn param_at(param_values: *const *const c_char, idx: usize) -> *const c_char {
    if param_values.is_null() {
        return std::ptr::null();
    }
    *param_values.add(idx)
}

fn bytes_preview(bytes: &[u8], max_len: usize) -> (String, bool, usize) {
    let total_len = bytes.len();
    let cut = total_len.min(max_len);
    let mut out = String::new();
    for &b in &bytes[..cut] {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0 => out.push_str("\\0"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                out.push_str("\\x");
                out.push_str(&format!("{:02x}", b));
            }
        }
    }
    (out, total_len > max_len, total_len)
}

unsafe fn is_play_queue_stmt(pg_stmt: *mut PgStmt) -> bool {
    if pg_stmt.is_null() {
        return false;
    }
    let sql_bytes = cstr_bytes((*pg_stmt).sql);
    let pg_sql_bytes = cstr_bytes((*pg_stmt).pg_sql);
    contains_icase_bytes(sql_bytes, b"play_queue") || contains_icase_bytes(pg_sql_bytes, b"play_queue")
}

unsafe fn trace_play_queue_params(
    pg_stmt: *mut PgStmt,
    param_values: *const *const c_char,
    phase: &str,
) {
    if !trace_play_queue_enabled() || !is_play_queue_stmt(pg_stmt) {
        return;
    }
    let param_count = (*pg_stmt).param_count;
    let count = if param_count > 0 { param_count as usize } else { 0 };
    let max_params = 16usize;
    let max_len = 256usize;
    log_info(&format!(
        "PLAY_QUEUE TRACE {}: param_count={} sql={:.200}",
        phase,
        param_count,
        cstr_to_str((*pg_stmt).pg_sql)
    ));
    if !(*pg_stmt).sql.is_null() && (*pg_stmt).sql != (*pg_stmt).pg_sql {
        log_info(&format!(
            "PLAY_QUEUE TRACE {}: sqlite_sql={:.200}",
            phase,
            cstr_to_str((*pg_stmt).sql)
        ));
    }
    if count == 0 {
        log_info(&format!("PLAY_QUEUE TRACE {} params: (none)", phase));
        return;
    }
    let mut parts: Vec<String> = Vec::with_capacity(count.min(max_params));
    for i in 0..count.min(max_params) {
        let val_ptr = param_at(param_values, i);
        let val_str = if val_ptr.is_null() {
            "NULL".to_string()
        } else {
            let bytes = CStr::from_ptr(val_ptr).to_bytes();
            let (preview, truncated, total_len) = bytes_preview(bytes, max_len);
            if truncated {
                format!("{}...(len={})", preview, total_len)
            } else {
                preview
            }
        };
        parts.push(format!("${}={}", i + 1, val_str));
    }
    log_info(&format!(
        "PLAY_QUEUE TRACE {} params: {}",
        phase,
        parts.join(", ")
    ));
    if count > max_params {
        log_info(&format!(
            "PLAY_QUEUE TRACE {} params: truncated {} of {}",
            phase, max_params, count
        ));
    }
}

unsafe fn trace_play_queue_result(
    pg_stmt: *mut PgStmt,
    result: *mut PGresult,
    phase: &str,
) {
    if !trace_play_queue_enabled() || !is_play_queue_stmt(pg_stmt) || result.is_null() {
        return;
    }
    let num_rows = crate::libpq_helpers::rust_pq_ntuples(result);
    let num_cols = crate::libpq_helpers::rust_pq_nfields(result);
    let max_rows = 5i32;
    let max_cols = 16i32;
    let max_len = 256usize;
    log_info(&format!(
        "PLAY_QUEUE TRACE {} result: rows={} cols={}",
        phase, num_rows, num_cols
    ));
    let rows = if num_rows > 0 { num_rows } else { 0 };
    let cols = if num_cols > 0 { num_cols } else { 0 };
    let row_cap = rows.min(max_rows);
    let col_cap = cols.min(max_cols);
    for r in 0..row_cap {
        let mut parts: Vec<String> = Vec::with_capacity(col_cap as usize);
        for c in 0..col_cap {
            let name_ptr = crate::db_interpose_helpers::rust_pg_result_col_name(result, c);
            let name = if name_ptr.is_null() {
                format!("col{}", c)
            } else {
                CStr::from_ptr(name_ptr).to_str().unwrap_or("?").to_string()
            };
            let mut buf = vec![0u8; max_len + 1];
            let len = crate::db_interpose_helpers::rust_pg_result_text_copy(
                result,
                r,
                c,
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            );
            let val = if len < 0 {
                "NULL".to_string()
            } else {
                let total_len = len as usize;
                let copy_len = total_len.min(buf.len().saturating_sub(1));
                let (preview, truncated, _) = bytes_preview(&buf[..copy_len], max_len);
                if truncated || total_len > copy_len {
                    format!("{}...(len={})", preview, total_len)
                } else {
                    preview
                }
            };
            parts.push(format!("{}={}", name, val));
        }
        log_info(&format!(
            "PLAY_QUEUE TRACE {} row {}: {}",
            phase,
            r,
            parts.join(", ")
        ));
    }
    if rows > row_cap {
        log_info(&format!(
            "PLAY_QUEUE TRACE {} result: truncated rows {} of {}",
            phase, row_cap, rows
        ));
    }
    if cols > col_cap {
        log_info(&format!(
            "PLAY_QUEUE TRACE {} result: truncated cols {} of {}",
            phase, col_cap, cols
        ));
    }
}

fn is_duplicate_prepared_stmt(res: *mut PGresult) -> bool {
    if res.is_null() {
        return false;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    crate::pg_client::rust_is_duplicate_sqlstate(sqlstate) != 0
}

fn is_stale_prepared_stmt(res: *mut PGresult) -> bool {
    if res.is_null() {
        return false;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    crate::pg_client::rust_is_stale_sqlstate(sqlstate) != 0
}

fn step_read_clear_row_caches(stmt: *mut PgStmt) {
    if stmt.is_null() {
        return;
    }
    unsafe {
        crate::db_interpose_helpers::rust_step_clear_row_caches(
            (*stmt).cached_text.as_mut_ptr(),
            (*stmt).cached_blob.as_mut_ptr(),
            (*stmt).cached_blob_len.as_mut_ptr(),
            (*stmt).decoded_blobs.as_mut_ptr(),
            (*stmt).decoded_blob_lens.as_mut_ptr(),
            MAX_PARAMS as c_int,
            &mut (*stmt).cached_row as *mut c_int,
            &mut (*stmt).decoded_blob_row as *mut c_int,
        );
    }
}

#[no_mangle]
pub extern "C" fn rust_step_read_advance_cached_result(stmt: *mut PgStmt) -> c_int {
    unsafe {
        if stmt.is_null() || (*stmt).cached_result.is_null() {
            return STEP_RESULT_ERROR;
        }
        let _stmt_guard = PthreadMutexGuard::lock(&mut (*stmt).mutex as *mut _);

        (*stmt).current_row += 1;
        if (*stmt).current_row >= (*stmt).num_rows {
            crate::pg_query_cache::rust_query_cache_release((*stmt).cached_result);
            (*stmt).cached_result = std::ptr::null_mut();
            (*stmt).read_done = 1;
            return STEP_RESULT_DONE;
        }
        STEP_RESULT_ROW
    }
}

#[no_mangle]
pub extern "C" fn rust_step_read_streaming_next(
    p_stmt: *mut sqlite3_stmt,
    stmt: *mut PgStmt,
) -> c_int {
    unsafe {
        if stmt.is_null()
            || (*stmt).streaming_mode == 0
            || (*stmt).streaming_conn.is_null()
            || (*(*stmt).streaming_conn).conn.is_null()
        {
            return STEP_RESULT_ERROR;
        }
        let mut stmt_guard = PthreadMutexGuard::lock(&mut (*stmt).mutex as *mut _);

        if !(*stmt).result.is_null() {
            crate::libpq_helpers::rust_pq_clear((*stmt).result);
            (*stmt).result = std::ptr::null_mut();
        }
        step_read_clear_row_caches(stmt);

        let row_res = crate::libpq_helpers::rust_pq_get_result((*(*stmt).streaming_conn).conn);
        if row_res.is_null() {
            log_error(&format!(
                "STREAM: NULL result (unexpected!) stmt={:p} sql={:.100} streaming_conn={:p}",
                p_stmt,
                cstr_to_str((*stmt).pg_sql),
                (*stmt).streaming_conn
            ));
            (*stmt).streaming_mode = 0;
            if !(*stmt).streaming_conn.is_null() {
                (*(*stmt).streaming_conn)
                    .streaming_active
                    .store(0, Ordering::SeqCst);
            }
            (*stmt).streaming_conn = std::ptr::null_mut();
            (*stmt).read_done = 1;
            return STEP_RESULT_DONE;
        }

        let row_status = crate::libpq_helpers::rust_pq_result_status(row_res);
        if row_status == PGRES_SINGLE_TUPLE {
            (*stmt).result = row_res;
            (*stmt).current_row = 0;
            (*stmt).num_rows = 1;
            (*stmt).num_cols = crate::libpq_helpers::rust_pq_nfields(row_res);
            trace_play_queue_result(stmt, row_res, "STREAM NEXT");
            stmt_guard.unlock();
            return STEP_RESULT_ROW;
        }
        if row_status == PGRES_TUPLES_OK {
            crate::libpq_helpers::rust_pq_clear(row_res);
            let final_null =
                crate::libpq_helpers::rust_pq_get_result((*(*stmt).streaming_conn).conn);
            if !final_null.is_null() {
                crate::libpq_helpers::rust_pq_clear(final_null);
            }
            (*stmt).streaming_mode = 0;
            if !(*stmt).streaming_conn.is_null() {
                (*(*stmt).streaming_conn)
                    .streaming_active
                    .store(0, Ordering::SeqCst);
            }
            (*stmt).streaming_conn = std::ptr::null_mut();
            (*stmt).read_done = 1;
            return STEP_RESULT_DONE;
        }

        let err = crate::libpq_helpers::rust_pq_error_message((*(*stmt).streaming_conn).conn);
        log_error(&format!(
            "STREAM ERROR: {} (status={}) sql={:.100}",
            cstr_to_str(err),
            row_status,
            cstr_to_str((*stmt).pg_sql)
        ));
        crate::libpq_helpers::rust_pq_clear(row_res);
        let mut drain = crate::libpq_helpers::rust_pq_get_result((*(*stmt).streaming_conn).conn);
        while !drain.is_null() {
            crate::libpq_helpers::rust_pq_clear(drain);
            drain = crate::libpq_helpers::rust_pq_get_result((*(*stmt).streaming_conn).conn);
        }
        (*stmt).streaming_mode = 0;
        if !(*stmt).streaming_conn.is_null() {
            (*(*stmt).streaming_conn)
                .streaming_active
                .store(0, Ordering::SeqCst);
        }
        (*stmt).streaming_conn = std::ptr::null_mut();
        (*stmt).read_done = 1;
        STEP_RESULT_DONE
    }
}

#[no_mangle]
pub extern "C" fn rust_step_read_eager_next(stmt: *mut PgStmt) -> c_int {
    unsafe {
        if stmt.is_null() || (*stmt).result.is_null() {
            return STEP_RESULT_ERROR;
        }
        let _stmt_guard = PthreadMutexGuard::lock(&mut (*stmt).mutex as *mut _);

        (*stmt).current_row += 1;
        if (*stmt).current_row >= (*stmt).num_rows {
            crate::libpq_helpers::rust_pq_clear((*stmt).result);
            (*stmt).result = std::ptr::null_mut();
            (*stmt).result_conn = std::ptr::null_mut();
            (*stmt).read_done = 1;
            return STEP_RESULT_DONE;
        }
        STEP_RESULT_ROW
    }
}

#[no_mangle]
pub extern "C" fn rust_step_read_first_execute(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    param_values: *const *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if pg_stmt.is_null() || exec_conn_io.is_null() {
            return STEP_RESULT_ERROR;
        }
        let mut stmt_guard = PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _);

        let mut exec_conn = *exec_conn_io;
        (*pg_stmt).executing_thread = libc::pthread_self();

        if exec_conn.is_null() || (*exec_conn).conn.is_null() {
            log_error(&format!(
                "STEP SELECT: NULL connection, retrying in 500ms (exec_conn={:p})",
                exec_conn
            ));
            stmt_guard.unlock();
            libc::usleep(500_000);
            stmt_guard = PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _);

            let retry_db = sqlite3_db_handle((*pg_stmt).shadow_stmt);
            let retry_handle = crate::pg_client::rust_pg_find_connection(retry_db);
            if !retry_handle.is_null() && (*retry_handle).db_path[0] != 0 {
                exec_conn =
                    crate::pg_client::rust_pool_get_connection((*retry_handle).db_path.as_ptr())
                        as *mut PgConnection;
            }
            if exec_conn.is_null() || (*exec_conn).conn.is_null() {
                log_error("STEP SELECT: NULL connection after retry - giving up");
                stmt_guard.unlock();
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            }
            log_error(&format!(
                "STEP SELECT: reconnect retry succeeded (exec_conn={:p})",
                exec_conn
            ));
        }

        crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
        let mut conn_guard = PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _);

        if (*exec_conn).conn.is_null() {
            log_error("STEP SELECT: conn became NULL after lock (TOCTOU race)");
            conn_guard.unlock();
            stmt_guard.unlock();
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        if (*exec_conn).streaming_active.load(Ordering::SeqCst) != 0 {
            log_info(&format!(
                "STEP SELECT: conn {:p} became streaming_active after lock, getting new connection",
                exec_conn
            ));
            conn_guard.unlock();
            let base_path = if !(*pg_stmt).conn.is_null() {
                (*(*pg_stmt).conn).db_path.as_ptr()
            } else {
                std::ptr::null()
            };
            let alt_conn = crate::pg_client::rust_pool_get_connection(base_path) as *mut PgConnection;
            if !alt_conn.is_null()
                && !(*alt_conn).conn.is_null()
                && alt_conn != exec_conn
                && (*alt_conn).streaming_active.load(Ordering::SeqCst) == 0
            {
                exec_conn = alt_conn;
                crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
                conn_guard = PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _);
                if (*exec_conn).conn.is_null()
                    || (*exec_conn).streaming_active.load(Ordering::SeqCst) != 0
                {
                    log_error("STEP SELECT: alt conn also unavailable");
                    conn_guard.unlock();
                    stmt_guard.unlock();
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }
            } else {
                log_error("STEP SELECT: no non-streaming connection available");
                stmt_guard.unlock();
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            }
        }

        let conn_status = crate::libpq_helpers::rust_pq_status((*exec_conn).conn);
        if conn_status != CONNECTION_OK {
            let pg_err = crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn);
            log_error("=== CONNECTION_BAD DIAGNOSTIC (READ) ===");
            log_error(&format!(
                "  Status: {}, Thread: {:p}",
                conn_status,
                libc::pthread_self() as usize as *const c_void
            ));
            log_error(&format!(
                "  Connection: {:p}, PGconn: {:p}",
                exec_conn, (*exec_conn).conn
            ));
            log_error(&format!("  PG Error: {}", cstr_to_str(pg_err)));
            log_error(&format!(
                "  SQL: {:.100}",
                cstr_to_str((*pg_stmt).sql)
            ));
            if let Ok(reason) = CString::new("CONNECTION_BAD in STEP READ") {
                platform_print_backtrace(reason.as_ptr(), 1);
            }
            log_error("=== END DIAGNOSTIC ===");
            log_error("STEP READ: Attempting PQreset...");
            crate::libpq_helpers::rust_pq_reset((*exec_conn).conn);
            if crate::libpq_helpers::rust_pq_status((*exec_conn).conn) != CONNECTION_OK {
                log_error("STEP READ: PQreset failed, trying fresh PQconnectdb...");
                crate::pg_client::rust_stmt_cache_clear(exec_conn as *mut c_void);
                crate::libpq_helpers::rust_pq_finish((*exec_conn).conn);
                (*exec_conn).conn = std::ptr::null_mut();

                let rcfg = pg_config_get();
                if rcfg.is_null() {
                    log_error("STEP READ: pg_config_get returned NULL");
                    (*exec_conn).is_pg_active = 0;
                    conn_guard.unlock();
                    stmt_guard.unlock();
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }
                let new_read_conn = connect_new(&*rcfg);
                if crate::libpq_helpers::rust_pq_status(new_read_conn) == CONNECTION_OK {
                    (*exec_conn).conn = new_read_conn;
                    (*exec_conn).is_pg_active = 1;
                    log_info("STEP READ: fresh connection succeeded (reconnected)");
                } else {
                    let reset_err = crate::libpq_helpers::rust_pq_error_message(new_read_conn);
                    log_error(&format!(
                        "STEP READ: fresh connection also failed: {}",
                        cstr_to_str(reset_err)
                    ));
                    crate::libpq_helpers::rust_pq_finish(new_read_conn);
                    (*exec_conn).is_pg_active = 0;
                    conn_guard.unlock();
                    stmt_guard.unlock();
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }
            } else {
                log_error("STEP READ: PQreset succeeded, connection recovered");
            }
            let cfg = pg_config_get();
            if !cfg.is_null() {
                apply_pg_session_settings((*exec_conn).conn, &*cfg);
            }
        }

        let scope = CString::new("STEP READ").unwrap();
        crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(exec_conn, scope.as_ptr());

        let timeout = CString::new(STATEMENT_TIMEOUT_SQL).unwrap();
        let to_res = crate::libpq_helpers::rust_pq_exec((*exec_conn).conn, timeout.as_ptr());
        if !to_res.is_null() {
            crate::libpq_helpers::rust_pq_clear(to_res);
        }

        if !(*pg_stmt).pg_sql.is_null() {
            pg_exception_note_query((*pg_stmt).pg_sql);
        }

        trace_play_queue_params(pg_stmt, param_values, "EXEC");

        log_debug(&format!(
            "PREPARED CHECK: use_prepared={} stmt_name[0]={} pg_sql={:p}",
            (*pg_stmt).use_prepared,
            (*pg_stmt).stmt_name[0] as i32,
            (*pg_stmt).pg_sql
        ));

        let send_ok = if (*pg_stmt).use_prepared != 0
            && (*pg_stmt).stmt_name[0] != 0
            && !(*pg_stmt).pg_sql.is_null()
        {
            let mut cached_name: *const c_char = std::ptr::null();
            let is_cached = crate::pg_client::rust_stmt_cache_lookup(
                exec_conn as *mut c_void,
                (*pg_stmt).sql_hash,
                &mut cached_name,
            ) != 0;

            let mut cached = is_cached;
            if !cached {
                let prep_res = crate::libpq_helpers::rust_pq_prepare(
                    (*exec_conn).conn,
                    (*pg_stmt).stmt_name.as_ptr(),
                    (*pg_stmt).pg_sql,
                    (*pg_stmt).param_count,
                    std::ptr::null(),
                );
                if crate::libpq_helpers::rust_pq_result_status(prep_res) == PGRES_COMMAND_OK {
                    crate::pg_client::rust_stmt_cache_add(
                        exec_conn as *mut c_void,
                        (*pg_stmt).sql_hash,
                        (*pg_stmt).stmt_name.as_ptr(),
                        (*pg_stmt).param_count,
                    );
                    cached_name = (*pg_stmt).stmt_name.as_ptr();
                    cached = true;
                } else if is_duplicate_prepared_stmt(prep_res) {
                    crate::pg_client::rust_stmt_cache_add(
                        exec_conn as *mut c_void,
                        (*pg_stmt).sql_hash,
                        (*pg_stmt).stmt_name.as_ptr(),
                        (*pg_stmt).param_count,
                    );
                    cached_name = (*pg_stmt).stmt_name.as_ptr();
                    cached = true;
                } else {
                    log_error(&format!(
                        "PQprepare failed for {}: {}",
                        cstr_to_str((*pg_stmt).stmt_name.as_ptr()),
                        cstr_to_str(crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn))
                    ));
                }
                crate::libpq_helpers::rust_pq_clear(prep_res);
            }

            if cached && !cached_name.is_null() {
                crate::libpq_helpers::rust_pq_send_query_prepared(
                    (*exec_conn).conn,
                    cached_name,
                    (*pg_stmt).param_count,
                    param_values,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            } else {
                crate::libpq_helpers::rust_pq_send_query_params(
                    (*exec_conn).conn,
                    (*pg_stmt).pg_sql,
                    (*pg_stmt).param_count,
                    std::ptr::null(),
                    param_values,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            }
        } else {
            crate::libpq_helpers::rust_pq_send_query_params(
                (*exec_conn).conn,
                (*pg_stmt).pg_sql,
                (*pg_stmt).param_count,
                std::ptr::null(),
                param_values,
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        };

        if send_ok == 0 {
            let err = crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn);
            log_error(&format!(
                "PQsend* failed: {} sql={:.200}",
                cstr_to_str(err),
                cstr_to_str((*pg_stmt).pg_sql)
            ));
            if !err.is_null() && cstr_to_str(err).contains("does not exist") {
                crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
            }
            conn_guard.unlock();
            crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
            stmt_guard.unlock();
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        let disable_streaming = {
            let key = CString::new("PLEX_PG_DISABLE_STREAMING").unwrap();
            let val = libc::getenv(key.as_ptr());
            crate::db_interpose_helpers::rust_env_truthy(val) != 0
        };

        let mut use_streaming = !disable_streaming;
        if disable_streaming {
            log_info("STREAM: disabled via PLEX_PG_DISABLE_STREAMING, using eager fetch");
        }

        if use_streaming {
            if crate::libpq_helpers::rust_pq_set_single_row_mode((*exec_conn).conn) == 0 {
                log_error("PQsetSingleRowMode failed, falling back to eager fetch");
                use_streaming = false;
            }
        }

        if use_streaming {
            (*pg_stmt).streaming_mode = 1;
            (*pg_stmt).streaming_conn = exec_conn;
            (*pg_stmt).result_conn = exec_conn;
            (*exec_conn)
                .streaming_active
                .store(1, Ordering::SeqCst);
            (*pg_stmt).metadata_only_result = 0;
            conn_guard.unlock();

            let first_res = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
            if first_res.is_null() {
                (*pg_stmt).streaming_mode = 0;
                if !(*pg_stmt).streaming_conn.is_null() {
                    (*(*pg_stmt).streaming_conn)
                        .streaming_active
                        .store(0, Ordering::SeqCst);
                }
                (*pg_stmt).streaming_conn = std::ptr::null_mut();
                (*pg_stmt).read_done = 1;
                *exec_conn_io = exec_conn;
                stmt_guard.unlock();
                return STEP_RESULT_DONE;
            }

            let first_status = crate::libpq_helpers::rust_pq_result_status(first_res);
            if first_status == PGRES_SINGLE_TUPLE {
                (*pg_stmt).result = first_res;
                (*pg_stmt).current_row = 0;
                (*pg_stmt).num_rows = 1;
                (*pg_stmt).num_cols = crate::libpq_helpers::rust_pq_nfields(first_res);
                resolve_column_tables(pg_stmt, exec_conn);
                trace_play_queue_result(pg_stmt, first_res, "STREAM FIRST");
                *exec_conn_io = exec_conn;
                stmt_guard.unlock();
                return STEP_RESULT_ROW;
            }
            if first_status == PGRES_TUPLES_OK {
                log_debug(&format!(
                    "STREAM: zero rows returned for sql={:.200}",
                    cstr_to_str((*pg_stmt).pg_sql)
                ));
                crate::libpq_helpers::rust_pq_clear(first_res);
                let final_null = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
                if !final_null.is_null() {
                    crate::libpq_helpers::rust_pq_clear(final_null);
                }
                (*pg_stmt).streaming_mode = 0;
                if !(*pg_stmt).streaming_conn.is_null() {
                    (*(*pg_stmt).streaming_conn)
                        .streaming_active
                        .store(0, Ordering::SeqCst);
                }
                (*pg_stmt).streaming_conn = std::ptr::null_mut();
                (*pg_stmt).num_cols = 0;
                (*pg_stmt).num_rows = 0;
                (*pg_stmt).read_done = 1;
                *exec_conn_io = exec_conn;
                stmt_guard.unlock();
                return STEP_RESULT_DONE;
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
            (*pg_stmt).streaming_mode = 0;
            if !(*pg_stmt).streaming_conn.is_null() {
                (*(*pg_stmt).streaming_conn)
                    .streaming_active
                    .store(0, Ordering::SeqCst);
            }
            (*pg_stmt).streaming_conn = std::ptr::null_mut();
            crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
            *exec_conn_io = exec_conn;
            stmt_guard.unlock();
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        // --- eager fetch ---
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
            log_sql_fallback((*pg_stmt).sql, (*pg_stmt).pg_sql, fallback_err.as_ptr(), ctx.as_ptr());
            crate::libpq_helpers::rust_pq_clear((*pg_stmt).result);
            (*pg_stmt).result = std::ptr::null_mut();
            crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
        }

        (*pg_stmt).read_done = 1;
        *exec_conn_io = exec_conn;
        stmt_guard.unlock();
        STEP_RESULT_DONE
    }
}

#[no_mangle]
pub extern "C" fn rust_step_read_log_debug_context(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) {
    if stmt.is_null() {
        return;
    }
    unsafe {
        if (*stmt).result.is_null() {
            log_debug(&format!(
                "STEP READ: thread={:p} stmt={:p} exec_conn={:p}",
                libc::pthread_self() as usize as *const c_void,
                stmt,
                exec_conn
            ));
        }
    }
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
        if (!(*stmt).result.is_null() || (*stmt).streaming_mode != 0) && (*stmt).result_conn != exec_conn {
            log_debug(&format!(
                "STEP: Re-executing on current thread's connection (stmt shared across threads, result_conn={:p} exec_conn={:p})",
                (*stmt).result_conn,
                exec_conn
            ));
            crate::pg_statement::rust_stmt_clear_result(stmt);
        }

        if !(*stmt).result.is_null() && (*stmt).metadata_only_result == 2 {
            log_debug("STEP: Clearing metadata-only result for re-execution with bound params");
            crate::libpq_helpers::rust_pq_clear((*stmt).result);
            (*stmt).result = std::ptr::null_mut();
            (*stmt).metadata_only_result = 0;
            (*stmt).current_row = -1;
        }
    }
}
