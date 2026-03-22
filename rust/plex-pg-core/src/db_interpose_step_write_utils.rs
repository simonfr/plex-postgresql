use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::byte_utils::{cstr_bytes, contains_bytes, contains_icase_bytes, starts_with_icase_bytes};
use crate::db_interpose_conn_utils::{
    apply_pg_session_settings, connect_new, cstr_prefix, cstr_to_string_or, log_debug, log_error,
    log_info, PthreadMutexGuard, PgConnConfig,
};
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt, STMT_NAME_LEN};
use crate::libpq_helpers::PGresult;

const SQLITE_DONE: c_int = 101;
const SQLITE_ERROR: c_int = 1;
const STEP_RESULT_DONE: c_int = SQLITE_DONE;
const STEP_RESULT_ERROR: c_int = SQLITE_ERROR;

const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const CONNECTION_OK: c_int = 0;
const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

static SKIP_STATS_RESOURCES_UPDATE: AtomicI32 = AtomicI32::new(-1);

extern "C" {
    fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3;
    fn log_sql_fallback(
        original_sql: *const c_char,
        translated_sql: *const c_char,
        error_msg: *const c_char,
        context: *const c_char,
    );
    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);
    fn pg_config_get() -> *mut PgConnConfig;
}

fn malloc_cstring(value: &str) -> *mut c_char {
    let bytes = value.as_bytes();
    unsafe {
        let ptr = libc::malloc(bytes.len() + 1) as *mut c_char;
        if ptr.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        *ptr.add(bytes.len()) = 0;
        ptr
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

fn skip_stats_resources_update() -> bool {
    let cached = SKIP_STATS_RESOURCES_UPDATE.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let flag = std::env::var("PLEX_PG_SKIP_STATS_RESOURCES_UPDATE")
        .ok()
        .and_then(|v| v.chars().next())
        .map(|c| c != '0')
        .unwrap_or(false);
    SKIP_STATS_RESOURCES_UPDATE.store(if flag { 1 } else { 0 }, Ordering::Relaxed);
    flag
}

unsafe fn param_at(param_values: *const *const c_char, idx: usize) -> *const c_char {
    if param_values.is_null() {
        return std::ptr::null();
    }
    *param_values.add(idx)
}

#[no_mangle]
pub extern "C" fn rust_step_pick_thread_connection(
    base_conn: *mut PgConnection,
) -> *mut PgConnection {
    unsafe {
        if base_conn.is_null() {
            return std::ptr::null_mut();
        }
        if crate::db_interpose_helpers::rust_is_library_or_blobs_db_path(
            (*base_conn).db_path.as_ptr(),
        ) == 0
        {
            return base_conn;
        }

        let thread_conn = crate::pg_client::rust_pool_get_connection(
            (*base_conn).db_path.as_ptr(),
        ) as *mut PgConnection;
        if !thread_conn.is_null()
            && (*thread_conn).is_pg_active != 0
            && !(*thread_conn).conn.is_null()
        {
            return thread_conn;
        }
        base_conn
    }
}

#[no_mangle]
pub extern "C" fn rust_step_cached_write_should_noop(
    base_conn: *mut PgConnection,
    sql: *const c_char,
    out_exec_conn: *mut *mut PgConnection,
) -> c_int {
    unsafe {
        let exec_conn = rust_step_pick_thread_connection(base_conn);
        if !out_exec_conn.is_null() {
            *out_exec_conn = exec_conn;
        }
        crate::db_interpose_txn_utils::rust_txn_terminator_should_noop(
            exec_conn,
            sql,
            std::ptr::null_mut(),
        )
    }
}

#[no_mangle]
pub extern "C" fn rust_step_pg_write_should_noop(
    exec_conn: *mut PgConnection,
    pg_sql: *const c_char,
    txn_state_out: *mut c_int,
) -> c_int {
    crate::db_interpose_txn_utils::rust_txn_terminator_should_noop(
        exec_conn,
        pg_sql,
        txn_state_out,
    )
}

#[no_mangle]
pub extern "C" fn rust_step_cached_write_build_exec_sql(
    orig_sql: *const c_char,
    translated_sql: *const c_char,
    exec_sql_out: *mut *const c_char,
) -> *mut c_char {
    unsafe {
        if !exec_sql_out.is_null() {
            *exec_sql_out = translated_sql;
        }
        if translated_sql.is_null() {
            return std::ptr::null_mut();
        }

        let owned = crate::pg_statement::rust_convert_metadata_settings_upsert(translated_sql);
        if !owned.is_null() {
            if !exec_sql_out.is_null() {
                *exec_sql_out = owned;
            }
            return owned;
        }

        let orig_bytes = cstr_bytes(orig_sql);
        let translated_bytes = cstr_bytes(translated_sql);

        if !orig_bytes.is_empty()
            && starts_with_icase_bytes(orig_bytes, b"INSERT")
            && contains_icase_bytes(translated_bytes, b"schema_migrations")
            && !contains_icase_bytes(translated_bytes, b"ON CONFLICT")
        {
            let base = cstr_to_string_or(translated_sql, "");
            let sql = format!("{base} ON CONFLICT DO NOTHING");
            let ptr = malloc_cstring(&sql);
            if !exec_sql_out.is_null() {
                *exec_sql_out = ptr;
            }
            return ptr;
        }

        if !orig_bytes.is_empty()
            && starts_with_icase_bytes(orig_bytes, b"INSERT")
            && !contains_bytes(translated_bytes, b"RETURNING")
            && !contains_icase_bytes(translated_bytes, b"schema_migrations")
        {
            let base = cstr_to_string_or(translated_sql, "");
            let sql = format!("{base} RETURNING id");
            let ptr = malloc_cstring(&sql);
            if !exec_sql_out.is_null() {
                *exec_sql_out = ptr;
            }
            return ptr;
        }

        std::ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn rust_step_write_should_skip_special_insert(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
) -> c_int {
    unsafe {
        if pg_stmt.is_null() || (*pg_stmt).pg_sql.is_null() {
            return 0;
        }

        let pg_sql_bytes = cstr_bytes((*pg_stmt).pg_sql);
        if contains_icase_bytes(pg_sql_bytes, b"statistics_media") {
            let count_val = if (*pg_stmt).param_count > 6 {
                param_at(param_values, 6)
            } else {
                std::ptr::null()
            };
            let duration_val = if (*pg_stmt).param_count > 7 {
                param_at(param_values, 7)
            } else {
                std::ptr::null()
            };
            let count_empty = count_val.is_null()
                || CStr::from_ptr(count_val).to_bytes() == b"0";
            let duration_empty = duration_val.is_null()
                || CStr::from_ptr(duration_val).to_bytes() == b"0";

            if count_empty && duration_empty {
                log_debug(&format!(
                    "SKIP statistics_media INSERT: count={} duration={} (empty)",
                    cstr_to_string_or(count_val, "NULL"),
                    cstr_to_string_or(duration_val, "NULL")
                ));

                if !exec_conn.is_null() && !(*exec_conn).conn.is_null() {
                    let _conn_guard =
                        PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _);
                    if (*exec_conn).conn.is_null() {
                        log_error("SKIP SEQ: conn became NULL after lock (TOCTOU race)");
                    } else if crate::libpq_helpers::rust_pq_status((*exec_conn).conn)
                        == CONNECTION_OK
                    {
                        let seq_res = crate::libpq_helpers::rust_pq_exec(
                            (*exec_conn).conn,
                            b"SELECT nextval('plex.statistics_media_id_seq')\0"
                                .as_ptr() as *const c_char,
                        );
                        if crate::libpq_helpers::rust_pq_result_status(seq_res)
                            == PGRES_TUPLES_OK
                            && crate::libpq_helpers::rust_pq_ntuples(seq_res) > 0
                        {
                            let mut seq_buf = [0 as c_char; 64];
                            let mut seq_val: *const c_char = std::ptr::null();
                            if crate::db_interpose_helpers::rust_pg_result_text_copy(
                                seq_res as *const crate::db_interpose_helpers::PGresult,
                                0,
                                0,
                                seq_buf.as_mut_ptr(),
                                seq_buf.len(),
                            ) >= 0
                            {
                                seq_val = seq_buf.as_ptr();
                            }
                            log_debug(&format!(
                                "SKIP: Advanced sequence to {}",
                                cstr_to_string_or(seq_val, "?")
                            ));
                        }
                        crate::libpq_helpers::rust_pq_clear(seq_res);
                    }
                }

                (*pg_stmt).write_executed = 1;
                return 1;
            }
        }

        if contains_icase_bytes(pg_sql_bytes, b"INSERT INTO")
            && contains_icase_bytes(pg_sql_bytes, b"metadata_items")
            && !contains_icase_bytes(pg_sql_bytes, b"metadata_item_settings")
            && !contains_icase_bytes(pg_sql_bytes, b"metadata_item_views")
            && !contains_icase_bytes(pg_sql_bytes, b"metadata_item_accounts")
            && !contains_icase_bytes(pg_sql_bytes, b"metadata_item_clusters")
        {
            let lib_col = CString::new("library_section_id").unwrap();
            let type_col = CString::new("metadata_type").unwrap();
            let lib_idx = crate::db_interpose_helpers::rust_find_insert_column_index(
                (*pg_stmt).pg_sql,
                lib_col.as_ptr(),
            );
            let type_idx = crate::db_interpose_helpers::rust_find_insert_column_index(
                (*pg_stmt).pg_sql,
                type_col.as_ptr(),
            );

            if lib_idx >= 0
                && type_idx >= 0
                && lib_idx < (*pg_stmt).param_count
                && type_idx < (*pg_stmt).param_count
            {
                let lib_val = param_at(param_values, lib_idx as usize);
                let type_val = param_at(param_values, type_idx as usize);
                if lib_val.is_null() && type_val.is_null() {
                    log_error(&format!(
                        "GUARD: Blocked junk INSERT into metadata_items (library_section_id=NULL, metadata_type=NULL) param_count={} lib_idx={} type_idx={}",
                        (*pg_stmt).param_count, lib_idx, type_idx
                    ));

                    if !exec_conn.is_null() && !(*exec_conn).conn.is_null() {
                        let _conn_guard =
                            PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _);
                        if !(*exec_conn).conn.is_null()
                            && crate::libpq_helpers::rust_pq_status((*exec_conn).conn)
                                == CONNECTION_OK
                        {
                            let seq_res = crate::libpq_helpers::rust_pq_exec(
                                (*exec_conn).conn,
                                b"SELECT nextval('plex.metadata_items_id_seq')\0"
                                    .as_ptr() as *const c_char,
                            );
                            if crate::libpq_helpers::rust_pq_result_status(seq_res)
                                == PGRES_TUPLES_OK
                                && crate::libpq_helpers::rust_pq_ntuples(seq_res) > 0
                            {
                                let mut seq_buf = [0 as c_char; 64];
                                let mut seq_val: *const c_char = std::ptr::null();
                                if crate::db_interpose_helpers::rust_pg_result_text_copy(
                                    seq_res as *const crate::db_interpose_helpers::PGresult,
                                    0,
                                    0,
                                    seq_buf.as_mut_ptr(),
                                    seq_buf.len(),
                                ) >= 0
                                {
                                    seq_val = seq_buf.as_ptr();
                                }
                                log_debug(&format!(
                                    "GUARD: Advanced metadata_items sequence to {}",
                                    cstr_to_string_or(seq_val, "?")
                                ));
                            }
                            crate::libpq_helpers::rust_pq_clear(seq_res);
                        }
                    }

                    (*pg_stmt).write_executed = 1;
                    return 1;
                }
            }
        }

        0
    }
}

#[no_mangle]
pub extern "C" fn rust_step_write_prepare_connection(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
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
        if exec_conn.is_null() || (*exec_conn).conn.is_null() {
            log_error(&format!(
                "STEP WRITE: NULL connection, retrying in 500ms (exec_conn={:p})",
                exec_conn
            ));
            stmt_guard.unlock();
            libc::usleep(500_000);
            stmt_guard = PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _);

            let retry_db = sqlite3_db_handle((*pg_stmt).shadow_stmt);
            let retry_handle = crate::pg_client::rust_pg_find_connection(retry_db);
            if !retry_handle.is_null() && (*retry_handle).db_path[0] != 0 {
                exec_conn = crate::pg_client::rust_pool_get_connection(
                    (*retry_handle).db_path.as_ptr(),
                ) as *mut PgConnection;
            }
            if exec_conn.is_null() || (*exec_conn).conn.is_null() {
                log_error("STEP WRITE: NULL connection after retry - giving up");
                (*pg_stmt).write_executed = 1;
                stmt_guard.unlock();
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            }
            log_error(&format!(
                "STEP WRITE: reconnect retry succeeded (exec_conn={:p})",
                exec_conn
            ));
        }

        crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
        let mut conn_guard = PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _);

        if (*exec_conn).conn.is_null() {
            log_error("STEP WRITE: conn became NULL after lock (TOCTOU race)");
            conn_guard.unlock();
            (*pg_stmt).write_executed = 1;
            stmt_guard.unlock();
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        if (*exec_conn)
            .streaming_active
            .load(Ordering::SeqCst)
            != 0
        {
            log_info(&format!(
                "STEP WRITE: conn {:p} became streaming_active after lock, getting new connection",
                exec_conn
            ));
            conn_guard.unlock();
            let db_path = if (*pg_stmt).conn.is_null() {
                std::ptr::null()
            } else {
                (*(*pg_stmt).conn).db_path.as_ptr()
            };
            let alt_conn = crate::pg_client::rust_pool_get_connection(db_path) as *mut PgConnection;
            if !alt_conn.is_null()
                && !(*alt_conn).conn.is_null()
                && alt_conn != exec_conn
                && (*alt_conn)
                    .streaming_active
                    .load(Ordering::SeqCst)
                    == 0
            {
                exec_conn = alt_conn;
                crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
                conn_guard = PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _);
                if (*exec_conn).conn.is_null()
                    || (*exec_conn)
                        .streaming_active
                        .load(Ordering::SeqCst)
                        != 0
                {
                    log_error("STEP WRITE: alt conn also unavailable");
                    conn_guard.unlock();
                    (*pg_stmt).write_executed = 1;
                    stmt_guard.unlock();
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }
            } else {
                log_error("STEP WRITE: no non-streaming connection available");
                (*pg_stmt).write_executed = 1;
                stmt_guard.unlock();
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            }
        }

        let write_conn_status = crate::libpq_helpers::rust_pq_status((*exec_conn).conn);
        if write_conn_status != CONNECTION_OK {
            let pg_err = crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn);
            log_error("=== CONNECTION_BAD DIAGNOSTIC (WRITE) ===");
            log_error(&format!(
                "  Status: {}, Thread: {:p}",
                write_conn_status,
                libc::pthread_self() as *mut c_void
            ));
            log_error(&format!(
                "  Connection: {:p}, PGconn: {:p}",
                exec_conn, (*exec_conn).conn
            ));
            log_error(&format!(
                "  PG Error: {}",
                cstr_to_string_or(pg_err, "(null)")
            ));
            log_error(&format!(
                "  SQL: {}",
                cstr_prefix((*pg_stmt).sql, 100, "(null)")
            ));
            platform_print_backtrace(
                b"CONNECTION_BAD in STEP WRITE\0".as_ptr() as *const c_char,
                1,
            );
            log_error("=== END DIAGNOSTIC ===");
            log_error("STEP WRITE: Attempting PQreset...");
            crate::libpq_helpers::rust_pq_reset((*exec_conn).conn);
            if crate::libpq_helpers::rust_pq_status((*exec_conn).conn) != CONNECTION_OK {
                log_error("STEP WRITE: PQreset failed, trying fresh PQconnectdb...");
                crate::pg_client::rust_stmt_cache_clear(exec_conn as *mut c_void);
                crate::libpq_helpers::rust_pq_finish((*exec_conn).conn);
                (*exec_conn).conn = std::ptr::null_mut();

                let cfg = pg_config_get();
                if cfg.is_null() {
                    log_error("STEP WRITE: pg_config_get returned NULL");
                    (*exec_conn).is_pg_active = 0;
                    conn_guard.unlock();
                    (*pg_stmt).write_executed = 1;
                    stmt_guard.unlock();
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }

                let new_write_conn = connect_new(&*cfg);
                if crate::libpq_helpers::rust_pq_status(new_write_conn) == CONNECTION_OK {
                    (*exec_conn).conn = new_write_conn;
                    (*exec_conn).is_pg_active = 1;
                    log_info("STEP WRITE: fresh connection succeeded (reconnected)");
                    apply_pg_session_settings((*exec_conn).conn, &*cfg);
                } else {
                    let reset_err = crate::libpq_helpers::rust_pq_error_message(new_write_conn);
                    log_error(&format!(
                        "STEP WRITE: fresh connection also failed: {}",
                        cstr_to_string_or(reset_err, "(null)")
                    ));
                    crate::libpq_helpers::rust_pq_finish(new_write_conn);
                    (*exec_conn).is_pg_active = 0;
                    conn_guard.unlock();
                    (*pg_stmt).write_executed = 1;
                    stmt_guard.unlock();
                    if !pg_conn_error_out.is_null() {
                        *pg_conn_error_out = 1;
                    }
                    return STEP_RESULT_ERROR;
                }
            } else {
                log_error("STEP WRITE: PQreset succeeded, connection recovered");
            }
        }

        let scope = CString::new("STEP WRITE").unwrap();
        crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(exec_conn, scope.as_ptr());
        *exec_conn_io = exec_conn;
        STEP_RESULT_DONE
    }
}

#[no_mangle]
pub extern "C" fn rust_step_write_execute_and_finalize(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if pg_stmt.is_null() || exec_conn.is_null() || (*exec_conn).conn.is_null() {
            if !pg_stmt.is_null() {
                (*pg_stmt).write_executed = 1;
            }
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }
        let mut conn_guard = PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _);

        if skip_stats_resources_update()
            && !(*pg_stmt).pg_sql.is_null()
            && starts_with_icase_bytes(cstr_bytes((*pg_stmt).pg_sql), b"UPDATE")
            && contains_icase_bytes(cstr_bytes((*pg_stmt).pg_sql), b"statistics_resources")
        {
            log_error("STEP WRITE: skipping statistics_resources UPDATE via PLEX_PG_SKIP_STATS_RESOURCES_UPDATE");
            conn_guard.unlock();
            (*pg_stmt).write_executed = 1;
            return STEP_RESULT_DONE;
        }

        let res: *mut PGresult = if (*pg_stmt).use_prepared != 0 && (*pg_stmt).stmt_name[0] != 0 {
            let mut cached_name: *const c_char = std::ptr::null();
            let mut is_cached = crate::pg_client::rust_stmt_cache_lookup(
                exec_conn as *mut c_void,
                (*pg_stmt).sql_hash,
                &mut cached_name,
            ) != 0;

            if !is_cached {
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
                    is_cached = true;
                } else if is_duplicate_prepared_stmt(prep_res) {
                    crate::pg_client::rust_stmt_cache_add(
                        exec_conn as *mut c_void,
                        (*pg_stmt).sql_hash,
                        (*pg_stmt).stmt_name.as_ptr(),
                        (*pg_stmt).param_count,
                    );
                    cached_name = (*pg_stmt).stmt_name.as_ptr();
                    is_cached = true;
                } else {
                    log_debug(&format!(
                        "PQprepare (write) failed for {}: {}",
                        cstr_to_string_or((*pg_stmt).stmt_name.as_ptr(), ""),
                        cstr_to_string_or(
                            crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn),
                            "(null)"
                        )
                    ));
                }
                crate::libpq_helpers::rust_pq_clear(prep_res);
            }

            if is_cached && !cached_name.is_null() {
                crate::libpq_helpers::rust_pq_exec_prepared(
                    (*exec_conn).conn,
                    cached_name,
                    (*pg_stmt).param_count,
                    param_values,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            } else {
                crate::libpq_helpers::rust_pq_exec_params(
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
            crate::libpq_helpers::rust_pq_exec_params(
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

        conn_guard.unlock();

        let status = crate::libpq_helpers::rust_pq_result_status(res);
        if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
            let cmd_tuples = crate::libpq_helpers::rust_pq_cmd_tuples(res);
            let tuples_ptr = if cmd_tuples.is_null() {
                b"1\0".as_ptr() as *const c_char
            } else {
                cmd_tuples
            };
            (*exec_conn).last_changes = crate::db_interpose_helpers::rust_pg_text_to_int(tuples_ptr);

            if status == PGRES_TUPLES_OK && crate::libpq_helpers::rust_pq_ntuples(res) > 0 {
                let mut id_buf = [0 as c_char; 64];
                let mut id_str: *const c_char = std::ptr::null();
                if crate::db_interpose_helpers::rust_pg_result_text_copy(
                    res as *const crate::db_interpose_helpers::PGresult,
                    0,
                    0,
                    id_buf.as_mut_ptr(),
                    id_buf.len(),
                ) >= 0
                {
                    id_str = id_buf.as_ptr();
                }
                if !id_str.is_null() && !CStr::from_ptr(id_str).to_bytes().is_empty() {
                    let rowid = crate::db_interpose_helpers::rust_pg_text_to_int64(id_str);
                    if rowid > 0 {
                        (*exec_conn).last_insert_rowid = rowid;
                        crate::pg_client::rust_set_global_last_insert_rowid(rowid);
                    }

                    if !(*pg_stmt).pg_sql.is_null()
                        && contains_bytes(cstr_bytes((*pg_stmt).pg_sql), b"play_queue_generators")
                    {
                        log_debug(&format!(
                            "STEP play_queue_generators: RETURNING id = {} on thread {:p} conn {:p}",
                            cstr_to_string_or(id_str, "?"),
                            libc::pthread_self() as *mut c_void,
                            exec_conn
                        ));
                    }
                    let meta_id = crate::pg_statement::rust_extract_metadata_id((*pg_stmt).sql);
                    if meta_id > 0 {
                        crate::pg_client::rust_set_global_metadata_id(meta_id);
                    }
                }
            }
        } else {
            let err = if !exec_conn.is_null() && !(*exec_conn).conn.is_null() {
                crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn)
            } else {
                b"NULL connection\0".as_ptr() as *const c_char
            };
            log_error(&format!(
                "STEP PG write error: {}",
                cstr_to_string_or(err, "NULL connection")
            ));
            log_error(&format!(
                "  Original SQL: {}",
                cstr_prefix((*pg_stmt).sql, 300, "(null)")
            ));
            log_error(&format!(
                "  Translated SQL: {}",
                cstr_prefix((*pg_stmt).pg_sql, 300, "(null)")
            ));
            if is_stale_prepared_stmt(res) {
                crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
                if !res.is_null() {
                    crate::libpq_helpers::rust_pq_clear(res);
                }
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                (*pg_stmt).write_executed = 1;
                return STEP_RESULT_ERROR;
            }
            crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
        }

        (*pg_stmt).write_executed = 1;
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        STEP_RESULT_DONE
    }
}

#[no_mangle]
pub extern "C" fn rust_step_cached_write_execute_and_finalize(
    cached_io: *mut *mut PgStmt,
    p_stmt: *mut sqlite3_stmt,
    changes_conn: *mut PgConnection,
    exec_conn: *mut PgConnection,
    orig_sql: *const c_char,
    exec_sql: *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    unsafe {
        if !pg_conn_error_out.is_null() {
            *pg_conn_error_out = 0;
        }
        if exec_conn.is_null()
            || (*exec_conn).conn.is_null()
            || orig_sql.is_null()
            || exec_sql.is_null()
        {
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        if contains_bytes(cstr_bytes(orig_sql), b"play_queue_generators") {
            log_debug(&format!(
                "CACHED INSERT play_queue_generators on thread {:p} conn {:p}",
                libc::pthread_self() as *mut c_void,
                exec_conn
            ));
        }

        crate::pg_client::rust_pool_touch_connection(exec_conn as *const c_void);
        let mut conn_guard = PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _);

        if (*exec_conn).conn.is_null() {
            log_error("CACHED EXEC: conn became NULL after lock (TOCTOU race)");
            conn_guard.unlock();
            if !pg_conn_error_out.is_null() {
                *pg_conn_error_out = 1;
            }
            return STEP_RESULT_ERROR;
        }

        let scope = CString::new("CACHED EXEC").unwrap();
        crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(exec_conn, scope.as_ptr());

        let sql_hash = crate::pg_client::rust_hash_sql(exec_sql);
        let stmt_name = format!("ce_{:x}", sql_hash);
        let mut stmt_name_buf = [0 as c_char; STMT_NAME_LEN];
        let bytes = stmt_name.as_bytes();
        let len = bytes.len().min(STMT_NAME_LEN.saturating_sub(1));
        for i in 0..len {
            stmt_name_buf[i] = bytes[i] as c_char;
        }
        stmt_name_buf[len] = 0;

        let mut cached_stmt_name: *const c_char = std::ptr::null();
        let res: *mut PGresult = if crate::pg_client::rust_stmt_cache_lookup(
            exec_conn as *mut c_void,
            sql_hash,
            &mut cached_stmt_name,
        ) != 0
        {
            crate::libpq_helpers::rust_pq_exec_prepared(
                (*exec_conn).conn,
                cached_stmt_name,
                0,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        } else {
            let prep_res = crate::libpq_helpers::rust_pq_prepare(
                (*exec_conn).conn,
                stmt_name_buf.as_ptr(),
                exec_sql,
                0,
                std::ptr::null(),
            );
            if crate::libpq_helpers::rust_pq_result_status(prep_res) == PGRES_COMMAND_OK {
                crate::pg_client::rust_stmt_cache_add(
                    exec_conn as *mut c_void,
                    sql_hash,
                    stmt_name_buf.as_ptr(),
                    0,
                );
                crate::libpq_helpers::rust_pq_clear(prep_res);
                crate::libpq_helpers::rust_pq_exec_prepared(
                    (*exec_conn).conn,
                    stmt_name_buf.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            } else if is_duplicate_prepared_stmt(prep_res) {
                crate::pg_client::rust_stmt_cache_add(
                    exec_conn as *mut c_void,
                    sql_hash,
                    stmt_name_buf.as_ptr(),
                    0,
                );
                crate::libpq_helpers::rust_pq_clear(prep_res);
                crate::libpq_helpers::rust_pq_exec_prepared(
                    (*exec_conn).conn,
                    stmt_name_buf.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            } else {
                log_debug(&format!(
                    "CACHED EXEC prepare failed, using PQexec: {}",
                    cstr_to_string_or(
                        crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn),
                        "(null)"
                    )
                ));
                crate::libpq_helpers::rust_pq_clear(prep_res);
                crate::libpq_helpers::rust_pq_exec((*exec_conn).conn, exec_sql)
            }
        };
        conn_guard.unlock();

        let status = crate::libpq_helpers::rust_pq_result_status(res);
        if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
            if !changes_conn.is_null() {
                let cmd_tuples = crate::libpq_helpers::rust_pq_cmd_tuples(res);
                let tuples_ptr = if cmd_tuples.is_null() {
                    b"1\0".as_ptr() as *const c_char
                } else {
                    cmd_tuples
                };
                (*changes_conn).last_changes =
                    crate::db_interpose_helpers::rust_pg_text_to_int(tuples_ptr);
            }

            if starts_with_icase_bytes(cstr_bytes(orig_sql), b"INSERT")
                && status == PGRES_TUPLES_OK
                && crate::libpq_helpers::rust_pq_ntuples(res) > 0
            {
                let mut id_buf = [0 as c_char; 64];
                let mut id_str: *const c_char = std::ptr::null();
                if crate::db_interpose_helpers::rust_pg_result_text_copy(
                    res as *const crate::db_interpose_helpers::PGresult,
                    0,
                    0,
                    id_buf.as_mut_ptr(),
                    id_buf.len(),
                ) >= 0
                {
                    id_str = id_buf.as_ptr();
                }
                if !id_str.is_null() && !CStr::from_ptr(id_str).to_bytes().is_empty() {
                    let meta_id = crate::pg_statement::rust_extract_metadata_id(orig_sql);
                    if meta_id > 0 {
                        crate::pg_client::rust_set_global_metadata_id(meta_id);
                    }
                }
            }
        } else {
            let err = if !changes_conn.is_null() && !(*changes_conn).conn.is_null() {
                crate::libpq_helpers::rust_pq_error_message((*changes_conn).conn)
            } else {
                b"NULL connection\0".as_ptr() as *const c_char
            };
            log_sql_fallback(orig_sql, exec_sql, err, b"CACHED WRITE\0".as_ptr() as *const c_char);
            if is_stale_prepared_stmt(res) {
                crate::pg_client::rust_stmt_cache_clear_local(exec_conn as *mut c_void);
                crate::libpq_helpers::rust_pq_clear(res);
                if !pg_conn_error_out.is_null() {
                    *pg_conn_error_out = 1;
                }
                return STEP_RESULT_ERROR;
            }
            crate::pg_client::rust_pool_check_health(exec_conn as *mut c_void);
        }

        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }

        let mut cached = if !cached_io.is_null() {
            *cached_io
        } else {
            std::ptr::null_mut()
        };
        if cached.is_null() {
            cached = crate::pg_statement::rust_stmt_create(exec_conn, orig_sql, p_stmt);
            if !cached.is_null() {
                (*cached).is_pg = 1;
                (*cached).is_cached = 1;
                (*cached).write_executed = 1;
                crate::pg_statement::rust_cached_stmt_register(p_stmt as usize, cached as usize);
            }
            if !cached_io.is_null() {
                *cached_io = cached;
            }
        } else {
            (*cached).write_executed = 1;
        }

        STEP_RESULT_DONE
    }
}

#[no_mangle]
pub extern "C" fn rust_step_write_log_debug_context(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
) {
    unsafe {
        if pg_stmt.is_null() {
            return;
        }

        if !(*pg_stmt).pg_sql.is_null()
            && contains_bytes(cstr_bytes((*pg_stmt).pg_sql), b"play_queue_generators")
        {
            log_debug(&format!(
                "INSERT play_queue_generators on thread {:p} conn {:p}",
                libc::pthread_self() as *mut c_void,
                exec_conn
            ));
        }

        if !(*pg_stmt).sql.is_null()
            && contains_icase_bytes(cstr_bytes((*pg_stmt).sql), b"INSERT INTO metadata_items")
        {
            let p0 = if (*pg_stmt).param_count > 0 {
                param_at(param_values, 0)
            } else {
                std::ptr::null()
            };
            let p1 = if (*pg_stmt).param_count > 1 {
                param_at(param_values, 1)
            } else {
                std::ptr::null()
            };
            let p2 = if (*pg_stmt).param_count > 2 {
                param_at(param_values, 2)
            } else {
                std::ptr::null()
            };
            let p8 = if (*pg_stmt).param_count > 8 {
                param_at(param_values, 8)
            } else {
                std::ptr::null()
            };
            let p9 = if (*pg_stmt).param_count > 9 {
                param_at(param_values, 9)
            } else {
                std::ptr::null()
            };
            log_debug(&format!(
                "STEP metadata_items INSERT: param_count={}",
                (*pg_stmt).param_count
            ));
            log_debug(&format!(
                "  PARAMS: [0]={} [1]={} [2]={} [8]={} [9]={}",
                cstr_to_string_or(p0, "NULL"),
                cstr_to_string_or(p1, "NULL"),
                cstr_to_string_or(p2, "NULL"),
                cstr_to_string_or(p8, "NULL"),
                cstr_to_string_or(p9, "NULL")
            ));
        }

        if !(*pg_stmt).sql.is_null()
            && contains_bytes(cstr_bytes((*pg_stmt).sql), b"play_queue_generators")
        {
            let p0 = if (*pg_stmt).param_count > 0 {
                param_at(param_values, 0)
            } else {
                std::ptr::null()
            };
            let p1 = if (*pg_stmt).param_count > 1 {
                param_at(param_values, 1)
            } else {
                std::ptr::null()
            };
            let p2 = if (*pg_stmt).param_count > 2 {
                param_at(param_values, 2)
            } else {
                std::ptr::null()
            };
            let p3 = if (*pg_stmt).param_count > 3 {
                param_at(param_values, 3)
            } else {
                std::ptr::null()
            };
            log_debug(&format!(
                "STEP play_queue_generators INSERT: param_count={}",
                (*pg_stmt).param_count
            ));
            log_debug(&format!(
                "  PARAMS: [0]={} [1]={} [2]={} [3]={}",
                cstr_to_string_or(p0, "NULL"),
                cstr_to_string_or(p1, "NULL"),
                cstr_to_string_or(p2, "NULL"),
                cstr_to_string_or(p3, "NULL")
            ));
            log_debug(&format!(
                "  SQL: {}",
                cstr_prefix((*pg_stmt).pg_sql, 300, "NULL")
            ));
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_step_log_step_exit_trace(pg_stmt: *mut PgStmt) {
    unsafe {
        if pg_stmt.is_null() || (*pg_stmt).pg_sql.is_null() {
            return;
        }
        let sql_bytes = cstr_bytes((*pg_stmt).pg_sql);
        let is_count = contains_bytes(sql_bytes, b"COUNT(")
            || contains_bytes(sql_bytes, b"SUM(")
            || contains_bytes(sql_bytes, b"MAX(");
        let is_playqueue = contains_bytes(sql_bytes, b"play_queue");

        if is_count || is_playqueue {
            log_debug(&format!(
                "DEBUG_TRACE: STEP_EXIT - rows={} cols={} sql={}",
                (*pg_stmt).num_rows,
                (*pg_stmt).num_cols,
                cstr_prefix((*pg_stmt).pg_sql, 100, "NULL")
            ));
        }
    }
}
