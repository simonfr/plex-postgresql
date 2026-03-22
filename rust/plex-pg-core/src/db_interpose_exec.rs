use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use crate::byte_utils::{contains_bytes, contains_icase_bytes, starts_with_icase_bytes};
use crate::db_interpose_conn_utils::{
    apply_pg_session_settings, connect_new, cstr_prefix, cstr_to_string_or, log_error, log_info,
    PthreadMutexGuard, PgConnConfig,
};
use crate::ffi_types::sqlite3;
use crate::libpq_helpers::PGresult;

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

const CONNECTION_OK: c_int = 0;
const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

const PG_RETRY_MAX_DELAYS: usize = 10;

type ExecCallback = Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;

thread_local! {
    static EXEC_RETRY_COUNT: Cell<i32> = const { Cell::new(0) };
    static EXEC_PG_CONN_ERROR: Cell<i32> = const { Cell::new(0) };
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
    static mut orig_sqlite3_exec: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_char,
            ExecCallback,
            *mut c_void,
            *mut *mut c_char,
        ) -> c_int,
    >;

    fn rewrite_blobs_schema_migrations(sql: *const c_char, db_path: *const c_char) -> *mut c_char;
    fn pg_config_get() -> *mut PgConnConfig;
    fn sql_translate(sql: *const c_char) -> SqlTranslation;
    fn sql_translation_free(result: *mut SqlTranslation);
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

fn orig_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    unsafe {
        match orig_sqlite3_exec {
            Some(f) => f(db, sql, callback, arg, errmsg),
            None => SQLITE_ERROR,
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    let rc = rust_my_sqlite3_exec_impl(db, sql, callback, arg, errmsg);

    let mut delays = [0i32; PG_RETRY_MAX_DELAYS];
    let mut max_retries = 0i32;
    crate::pg_config::pg_config_get_retry_delays(delays.as_mut_ptr(), &mut max_retries);

    let retry_count = EXEC_RETRY_COUNT.with(|c| c.get());
    let conn_error = EXEC_PG_CONN_ERROR.with(|c| c.get());

    if rc == SQLITE_ERROR && retry_count < max_retries && conn_error != 0 {
        EXEC_PG_CONN_ERROR.with(|c| c.set(0));
        let delay = delays[retry_count as usize];
        let new_count = retry_count + 1;
        EXEC_RETRY_COUNT.with(|c| c.set(new_count));
        log_error(&format!(
            "exec: PG conn error, retry {}/{} in {}ms (thread {:p})",
            new_count,
            max_retries,
            delay,
            unsafe { libc::pthread_self() } as *mut c_void
        ));

        let delay_ms = if delay < 0 { 0 } else { delay as u32 };
        unsafe {
            libc::usleep(delay_ms.saturating_mul(1000));
        }

        EXEC_PG_CONN_ERROR.with(|c| c.set(0));
        let retry_rc = rust_my_sqlite3_exec(db, sql, callback, arg, errmsg);

        if new_count > 0 && retry_rc != SQLITE_ERROR {
            log_error(&format!(
                "exec: retry succeeded after {} attempt(s)",
                new_count
            ));
        }
        EXEC_RETRY_COUNT.with(|c| c.set(0));
        return retry_rc;
    }

    if retry_count > 0 {
        if rc == SQLITE_ERROR {
            log_error("exec: retries exhausted, returning SQLITE_ERROR");
        }
        EXEC_RETRY_COUNT.with(|c| c.set(0));
    }

    rc
}

fn rust_my_sqlite3_exec_impl(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    if sql.is_null() {
        log_error("exec called with NULL SQL");
        return orig_exec(db, sql, callback, arg, errmsg);
    }

    let pg_conn = crate::pg_client::rust_pg_find_connection(db);

    if !pg_conn.is_null() && unsafe { (*pg_conn).is_pg_active } != 0 {
        unsafe {
            if (*pg_conn).conn.is_null()
                || crate::libpq_helpers::rust_pq_status((*pg_conn).conn) != CONNECTION_OK
            {
                log_error(&format!(
                    "EXEC: CONNECTION_BAD pre-flight, attempting reconnect (thread {:p})",
                    libc::pthread_self() as *mut c_void
                ));
                let mut conn_guard =
                    PthreadMutexGuard::lock(&mut (*pg_conn).mutex as *mut _);
                if !(*pg_conn).conn.is_null() {
                    crate::libpq_helpers::rust_pq_reset((*pg_conn).conn);
                    if crate::libpq_helpers::rust_pq_status((*pg_conn).conn) != CONNECTION_OK {
                        log_error("EXEC: PQreset failed, trying fresh PQconnectdb...");
                        crate::pg_client::rust_stmt_cache_clear(pg_conn as *mut c_void);
                        crate::libpq_helpers::rust_pq_finish((*pg_conn).conn);
                        (*pg_conn).conn = std::ptr::null_mut();

                        let rcfg = pg_config_get();
                        if rcfg.is_null() {
                            (*pg_conn).is_pg_active = 0;
                            conn_guard.unlock();
                            EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                            return SQLITE_ERROR;
                        }
                        let cfg = &*rcfg;
                        let new_conn = connect_new(cfg);
                        if crate::libpq_helpers::rust_pq_status(new_conn) == CONNECTION_OK {
                            (*pg_conn).conn = new_conn;
                            (*pg_conn).is_pg_active = 1;
                            log_info("EXEC: fresh connection succeeded (reconnected)");
                            apply_pg_session_settings((*pg_conn).conn, cfg);
                        } else {
                            log_error(&format!(
                                "EXEC: fresh connection also failed: {}",
                                cstr_to_string_or(
                                    crate::libpq_helpers::rust_pq_error_message(new_conn),
                                    "(null)"
                                )
                            ));
                            crate::libpq_helpers::rust_pq_finish(new_conn);
                            (*pg_conn).is_pg_active = 0;
                            conn_guard.unlock();
                            EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                            return SQLITE_ERROR;
                        }
                    } else {
                        log_error("EXEC: PQreset succeeded, connection recovered");
                    }
                    let cfg = pg_config_get();
                    if !cfg.is_null() {
                        apply_pg_session_settings((*pg_conn).conn, &*cfg);
                    }
                } else {
                    let rcfg = pg_config_get();
                    if rcfg.is_null() {
                        (*pg_conn).is_pg_active = 0;
                        conn_guard.unlock();
                        EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                        return SQLITE_ERROR;
                    }
                    let cfg = &*rcfg;
                    let new_conn = connect_new(cfg);
                    if crate::libpq_helpers::rust_pq_status(new_conn) == CONNECTION_OK {
                        (*pg_conn).conn = new_conn;
                        (*pg_conn).is_pg_active = 1;
                        log_error("EXEC: fresh connection from NULL succeeded");
                        let cfg2 = pg_config_get();
                        if !cfg2.is_null() {
                            apply_pg_session_settings((*pg_conn).conn, &*cfg2);
                        }
                    } else {
                        log_error(&format!(
                            "EXEC: fresh connection from NULL failed: {}",
                            cstr_to_string_or(
                                crate::libpq_helpers::rust_pq_error_message(new_conn),
                                "(null)"
                            )
                        ));
                        crate::libpq_helpers::rust_pq_finish(new_conn);
                        (*pg_conn).is_pg_active = 0;
                        conn_guard.unlock();
                        EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                        return SQLITE_ERROR;
                    }
                }
                conn_guard.unlock();
            }
        }

        let mut exec_sql = sql;
        let blobs_rewrite = unsafe { rewrite_blobs_schema_migrations(sql, (*pg_conn).db_path.as_ptr()) };
        if !blobs_rewrite.is_null() {
            exec_sql = blobs_rewrite;
        }

        if crate::pg_config::pg_config_should_skip_sql(exec_sql) == 0 {
            if crate::db_interpose_helpers::rust_is_junk_metadata_insert(exec_sql) != 0 {
                log_error(
                    "GUARD: Blocked exec junk INSERT into metadata_items (library_section_id=NULL, metadata_type=NULL)",
                );
                if !blobs_rewrite.is_null() {
                    unsafe { libc::free(blobs_rewrite as *mut c_void) };
                }
                return SQLITE_OK;
            }

            let mut trans = unsafe { sql_translate(exec_sql) };
            if trans.success != 0 && !trans.sql.is_null() {
                let mut owned_insert: *mut c_char = std::ptr::null_mut();
                let mut exec_pg_sql = trans.sql;
                let sql_bytes = unsafe { CStr::from_ptr(exec_sql).to_bytes() };

                if starts_with_icase_bytes(sql_bytes, b"INSERT")
                    && !contains_bytes(unsafe { CStr::from_ptr(trans.sql).to_bytes() }, b"RETURNING")
                {
                    let base = cstr_to_string_or(trans.sql, "");
                    let sql = format!("{base} RETURNING id");
                    owned_insert = malloc_cstring(&sql);
                    if !owned_insert.is_null() {
                        exec_pg_sql = owned_insert;
                        if contains_bytes(sql_bytes, b"play_queue_generators") {
                            log_info(&format!(
                                "EXEC play_queue_generators INSERT with RETURNING: {}",
                                cstr_prefix(exec_pg_sql, 300, "NULL")
                            ));
                        }
                    }
                }

                let mut conn_guard = unsafe {
                    PthreadMutexGuard::lock(&mut (*pg_conn).mutex as *mut _)
                };

                let normalized = crate::db_interpose_helpers::rust_normalize_sql_literals(exec_pg_sql);
                let res: *mut PGresult = if !normalized.is_null() {
                    let norm = unsafe { &*normalized };
                    let norm_hash = crate::pg_client::rust_hash_sql(norm.normalized_sql);
                    let mut cached_stmt_name: *const c_char = std::ptr::null();

                    if crate::pg_client::rust_stmt_cache_lookup(
                        pg_conn as *mut c_void,
                        norm_hash,
                        &mut cached_stmt_name,
                    ) != 0
                    {
                        crate::libpq_helpers::rust_pq_exec_prepared(
                            unsafe { (*pg_conn).conn },
                            cached_stmt_name,
                            norm.param_count,
                            norm.param_values as *const *const c_char,
                            std::ptr::null(),
                            std::ptr::null(),
                            0,
                        )
                    } else {
                        let stmt_name = format!("nx_{:x}", norm_hash);
                        let stmt_name_c = CString::new(stmt_name)
                            .unwrap_or_else(|_| CString::new("").unwrap());
                        let prep_res = crate::libpq_helpers::rust_pq_prepare(
                            unsafe { (*pg_conn).conn },
                            stmt_name_c.as_ptr(),
                            norm.normalized_sql,
                            0,
                            std::ptr::null(),
                        );
                        let ok = crate::libpq_helpers::rust_pq_result_status(prep_res) == PGRES_COMMAND_OK
                            || is_duplicate_prepared_stmt(prep_res);
                        if ok {
                            crate::pg_client::rust_stmt_cache_add(
                                pg_conn as *mut c_void,
                                norm_hash,
                                stmt_name_c.as_ptr(),
                                norm.param_count,
                            );
                            crate::libpq_helpers::rust_pq_clear(prep_res);
                            crate::libpq_helpers::rust_pq_exec_prepared(
                                unsafe { (*pg_conn).conn },
                                stmt_name_c.as_ptr(),
                                norm.param_count,
                                norm.param_values as *const *const c_char,
                                std::ptr::null(),
                                std::ptr::null(),
                                0,
                            )
                        } else {
                            crate::libpq_helpers::rust_pq_clear(prep_res);
                            crate::libpq_helpers::rust_pq_exec(unsafe { (*pg_conn).conn }, exec_pg_sql)
                        }
                    }
                } else {
                    let sql_hash = crate::pg_client::rust_hash_sql(exec_pg_sql);
                    let mut cached_stmt_name: *const c_char = std::ptr::null();
                    if crate::pg_client::rust_stmt_cache_lookup(
                        pg_conn as *mut c_void,
                        sql_hash,
                        &mut cached_stmt_name,
                    ) != 0
                    {
                        crate::libpq_helpers::rust_pq_exec_prepared(
                            unsafe { (*pg_conn).conn },
                            cached_stmt_name,
                            0,
                            std::ptr::null(),
                            std::ptr::null(),
                            std::ptr::null(),
                            0,
                        )
                    } else {
                        crate::libpq_helpers::rust_pq_exec(unsafe { (*pg_conn).conn }, exec_pg_sql)
                    }
                };

                if !normalized.is_null() {
                    crate::db_interpose_helpers::rust_free_normalized_sql(normalized);
                }

                let status = crate::libpq_helpers::rust_pq_result_status(res);
                if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
                    let cmd_tuples = crate::libpq_helpers::rust_pq_cmd_tuples(res);
                    let tuples_ptr = if cmd_tuples.is_null() {
                        c"1".as_ptr()
                    } else {
                        cmd_tuples
                    };
                    unsafe {
                        (*pg_conn).last_changes =
                            crate::db_interpose_helpers::rust_pg_text_to_int(tuples_ptr);
                    }

                    if starts_with_icase_bytes(sql_bytes, b"INSERT")
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
                        if !id_str.is_null()
                            && unsafe { !CStr::from_ptr(id_str).to_bytes().is_empty() }
                        {
                            if contains_bytes(sql_bytes, b"play_queue_generators") {
                                log_info(&format!(
                                    "EXEC play_queue_generators: RETURNING id = {}",
                                    cstr_to_string_or(id_str, "?")
                                ));
                            }
                            let meta_id =
                                crate::pg_statement::rust_extract_metadata_id(exec_sql);
                            if meta_id > 0 {
                                crate::pg_client::rust_set_global_metadata_id(meta_id);
                            }
                        }
                    }
                } else {
                    let err = if unsafe { (*pg_conn).conn }.is_null() {
                        c"NULL connection".as_ptr()
                    } else {
                        crate::libpq_helpers::rust_pq_error_message(unsafe { (*pg_conn).conn })
                    };
                    log_error(&format!(
                        "PostgreSQL exec error: {}",
                        cstr_to_string_or(err, "NULL connection")
                    ));
                    let is_conn_error = unsafe { (*pg_conn).conn }.is_null()
                        || crate::libpq_helpers::rust_pq_status(unsafe { (*pg_conn).conn })
                            != CONNECTION_OK;
                    let is_stale_stmt = is_stale_prepared_stmt(res);
                    if is_stale_stmt {
                        crate::pg_client::rust_stmt_cache_clear_local(pg_conn as *mut c_void);
                    }
                    crate::pg_client::rust_pool_check_health(pg_conn as *mut c_void);
                    if is_conn_error || is_stale_stmt {
                        if !owned_insert.is_null() {
                            unsafe { libc::free(owned_insert as *mut c_void) };
                        }
                        crate::libpq_helpers::rust_pq_clear(res);
                        unsafe {
                            conn_guard.unlock();
                        }
                        unsafe { sql_translation_free(&mut trans as *mut SqlTranslation) };
                        if !blobs_rewrite.is_null() {
                            unsafe { libc::free(blobs_rewrite as *mut c_void) };
                        }
                        EXEC_PG_CONN_ERROR.with(|c| c.set(1));
                        return SQLITE_ERROR;
                    }
                }

                if !owned_insert.is_null() {
                    unsafe { libc::free(owned_insert as *mut c_void) };
                }
                crate::libpq_helpers::rust_pq_clear(res);
                unsafe {
                    conn_guard.unlock();
                }
            }
            unsafe { sql_translation_free(&mut trans as *mut SqlTranslation) };
        }

        if !blobs_rewrite.is_null() {
            unsafe { libc::free(blobs_rewrite as *mut c_void) };
        }
        return SQLITE_OK;
    }

    let mut cleaned_sql: *mut c_char = std::ptr::null_mut();
    let mut exec_sql = sql;
    let sql_bytes = unsafe { CStr::from_ptr(sql).to_bytes() };
    if contains_icase_bytes(sql_bytes, b"collate icu_root") {
        cleaned_sql = crate::db_interpose_helpers::rust_strip_collate_icu_root(sql);
        if !cleaned_sql.is_null() {
            exec_sql = cleaned_sql;
        }
    }

    let rc = orig_exec(db, exec_sql, callback, arg, errmsg);
    if !cleaned_sql.is_null() {
        crate::db_interpose_helpers::rust_free_cstring(cleaned_sql);
    }
    rc
}
