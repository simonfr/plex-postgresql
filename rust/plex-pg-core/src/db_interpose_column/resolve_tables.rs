use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

pub(super) fn resolve_column_tables_impl(
    pg_stmt: *mut PgStmt,
    pg_conn: *mut PgConnection,
) -> c_int {
    struct ResolveGuard(*mut c_int);
    impl Drop for ResolveGuard {
        fn drop(&mut self) {
            unsafe {
                *self.0 = 0;
            }
        }
    }
    let flag = tls_in_resolve_tables_ptr();
    unsafe {
        if *flag != 0 {
            log_debug("RESOLVE_TABLES: Recursion detected, aborting");
            if !pg_stmt.is_null() {
                (*pg_stmt).col_tables_resolved = 1;
            }
            return -1;
        }
        *flag = 1;
    }
    let _guard = ResolveGuard(flag);

    if pg_stmt.is_null() {
        return 0;
    }

    unsafe {
        if (*pg_stmt).result.is_null() || (*pg_stmt).col_tables_resolved != 0 {
            return 0;
        }
    }

    let num_cols = unsafe { (*pg_stmt).num_cols };
    if num_cols <= 0 || num_cols as usize > MAX_COLS {
        unsafe { (*pg_stmt).col_tables_resolved = 1 };
        return 0;
    }

    let mut table_oids = [0u32; MAX_COLS];
    let mut uncached_oids = [0usize; MAX_COLS];
    let mut num_unique_tables = 0usize;
    let mut num_uncached = 0usize;
    let mut cache_hits = 0usize;

    for i in 0..(num_cols as usize) {
        let table_oid = unsafe {
            crate::db_interpose_helpers::rust_pg_result_col_table_oid(
                helpers_result_ptr((*pg_stmt).result),
                i as c_int,
            )
        };
        if table_oid == INVALID_OID {
            continue;
        }

        let cached_name = crate::db_interpose_helpers::rust_oid_table_cache_lookup(table_oid);
        if !cached_name.is_null() {
            let dup = unsafe { libc::strdup(cached_name) };
            if !dup.is_null() {
                unsafe { (*pg_stmt).col_table_names[i] = dup };
                cache_hits += 1;
            }
            continue;
        }

        let mut found = false;
        for j in 0..num_unique_tables {
            if table_oids[j] == table_oid {
                found = true;
                break;
            }
        }
        if !found && num_unique_tables < MAX_COLS {
            table_oids[num_unique_tables] = table_oid;
            uncached_oids[num_uncached] = num_unique_tables;
            num_unique_tables += 1;
            num_uncached += 1;
        }
    }

    if num_uncached == 0 {
        unsafe { (*pg_stmt).col_tables_resolved = 1 };
        if cache_hits > 0 {
            log_debug_lazy!(
                "RESOLVE_TABLES: All {} columns resolved from cache (0 queries)",
                cache_hits
            );
        }
        return 0;
    }

    if pg_conn.is_null() || unsafe { (*pg_conn).conn.is_null() } {
        log_debug("RESOLVE_TABLES: No connection available");
        unsafe { (*pg_stmt).col_tables_resolved = 1 };
        return -1;
    }

    let mut resolve_conn = pg_conn;
    if unsafe { (*pg_conn).streaming_active.load(Ordering::SeqCst) != 0 } {
        log_debug_lazy!(
            "RESOLVE_TABLES: Connection {:p} is streaming_active",
            pg_conn
        );
        if !env_utils::env_truthy_str("PLEX_PG_ENABLE_RESOLVE_TABLES_ALT_CONN") {
            log_debug(
                "RESOLVE_TABLES: skipping OID lookup while streaming to avoid reentrant alternate connection acquisition",
            );
            unsafe { (*pg_stmt).col_tables_resolved = 1 };
            return 0;
        }

        log_debug("RESOLVE_TABLES: alternate connection lookup explicitly enabled");
        let alt_conn = unsafe {
            pg_get_thread_connection_excluding(
                (*pg_conn).db_path.as_ptr(),
                pg_conn as *const c_void,
            )
        };
        if alt_conn.is_null()
            || unsafe { (*alt_conn).conn.is_null() }
            || alt_conn == pg_conn
            || unsafe { (*alt_conn).streaming_active.load(Ordering::SeqCst) != 0 }
        {
            log_debug(
                "RESOLVE_TABLES: No alternate connection, skipping OID lookup to protect streaming",
            );
            unsafe { (*pg_stmt).col_tables_resolved = 1 };
            return 0;
        }

        resolve_conn = alt_conn;
        log_debug_lazy!(
            "RESOLVE_TABLES: Using alternate connection {:p} for OID lookup",
            resolve_conn
        );
    }

    let mut query = String::from("SELECT oid, relname FROM pg_class WHERE oid IN (");
    for i in 0..num_unique_tables {
        if i > 0 {
            query.push(',');
        }
        query.push_str(&format!("{}", table_oids[i]));
    }
    query.push(')');
    let query_cs = match CString::new(query) {
        Ok(cs) => cs,
        Err(_) => {
            log_error("RESOLVE_TABLES: failed to build OID query");
            unsafe { (*pg_stmt).col_tables_resolved = 1 };
            return -1;
        }
    };

    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut (*resolve_conn).mutex as *mut _) };
    let res =
        unsafe { crate::libpq_helpers::rust_pq_exec((*resolve_conn).conn, query_cs.as_ptr()) };

    if res.is_null() || crate::libpq_helpers::rust_pq_result_status(res) != PGRES_TUPLES_OK {
        log_error(&format!(
            "RESOLVE_TABLES: Query failed: {}",
            if res.is_null() {
                "NULL result".to_string()
            } else {
                cstr_to_string_or(
                    unsafe { crate::libpq_helpers::rust_pq_error_message((*resolve_conn).conn) },
                    "?",
                )
            }
        ));
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        unsafe { (*pg_stmt).col_tables_resolved = 1 };
        return -1;
    }

    let num_results = crate::libpq_helpers::rust_pq_ntuples(res);
    let mut result_oids = vec![0u32; MAX_COLS];
    let mut result_names: Vec<[c_char; 64]> = vec![[0 as c_char; 64]; MAX_COLS];

    for i in 0..(num_results as usize).min(MAX_COLS) {
        let mut oid_buf = [0 as c_char; 64];
        let ok_oid = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(res),
            i as c_int,
            0,
            oid_buf.as_mut_ptr(),
            oid_buf.len(),
        );
        if ok_oid < 0 {
            continue;
        }
        let oid_val = unsafe { libc::atol(oid_buf.as_ptr()) } as u32;
        result_oids[i] = oid_val;

        let ok_name = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(res),
            i as c_int,
            1,
            result_names[i].as_mut_ptr(),
            result_names[i].len(),
        );
        if ok_name < 0 {
            result_names[i][0] = 0;
            continue;
        }

        crate::db_interpose_helpers::rust_oid_table_cache_insert(oid_val, result_names[i].as_ptr());
    }

    crate::libpq_helpers::rust_pq_clear(res);

    for i in 0..(num_cols as usize).min(MAX_COLS) {
        if unsafe { (*pg_stmt).col_table_names[i] }.is_null() {
            let table_oid = unsafe {
                crate::db_interpose_helpers::rust_pg_result_col_table_oid(
                    helpers_result_ptr((*pg_stmt).result),
                    i as c_int,
                )
            };
            if table_oid == INVALID_OID {
                continue;
            }
            for j in 0..(num_results as usize).min(MAX_COLS) {
                if result_oids[j] == table_oid {
                    let dup = unsafe { libc::strdup(result_names[j].as_ptr()) };
                    if !dup.is_null() {
                        unsafe { (*pg_stmt).col_table_names[i] = dup };
                        log_debug_lazy!(
                            "RESOLVE_TABLES: col[{}] '{}' -> table '{}'",
                            i,
                            cstr_to_string_or(
                                unsafe {
                                    crate::db_interpose_helpers::rust_pg_result_col_name(
                                        helpers_result_ptr((*pg_stmt).result),
                                        i as c_int,
                                    )
                                },
                                "?",
                            ),
                            cstr_to_string_or(result_names[j].as_ptr(), "?")
                        );
                    }
                    break;
                }
            }
        }
    }

    unsafe { (*pg_stmt).col_tables_resolved = 1 };
    log_info_lazy!(
        "RESOLVE_TABLES: Resolved {} columns ({} from cache, {} from query)",
        num_cols, cache_hits, num_unique_tables
    );
    0
}
