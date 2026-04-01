use super::*;
use crate::log_debug_lazy;

#[no_mangle]
pub extern "C" fn rust_step_write_should_skip_special_insert(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
) -> c_int {
    if pg_stmt.is_null() {
        return 0;
    }
    let stmt = unsafe { &mut *pg_stmt };
    if stmt.pg_sql.is_null() {
        return 0;
    }

    unsafe {
        let pg_sql_bytes = cstr_bytes(stmt.pg_sql);
        if contains_icase_bytes(pg_sql_bytes, b"statistics_media") {
            let count_val = if stmt.param_count > 6 {
                param_at(param_values, 6)
            } else {
                std::ptr::null()
            };
            let duration_val = if stmt.param_count > 7 {
                param_at(param_values, 7)
            } else {
                std::ptr::null()
            };
            let count_empty = count_val.is_null() || CStr::from_ptr(count_val).to_bytes() == b"0";
            let duration_empty =
                duration_val.is_null() || CStr::from_ptr(duration_val).to_bytes() == b"0";

            if count_empty && duration_empty {
                log_debug_lazy!(
                    "SKIP statistics_media INSERT: count={} duration={} (empty)",
                    cstr_to_string_or(count_val, "NULL"),
                    cstr_to_string_or(duration_val, "NULL")
                );

                if !exec_conn.is_null() && !(&*exec_conn).conn.is_null() {
                    let ec = &mut *exec_conn;
                    let _conn_guard = PthreadMutexGuard::lock(&mut ec.mutex as *mut _);
                    if ec.conn.is_null() {
                        log_error("SKIP SEQ: conn became NULL after lock (TOCTOU race)");
                    } else if crate::libpq_helpers::rust_pq_status(ec.conn) == CONNECTION_OK {
                        let seq_res = crate::libpq_helpers::rust_pq_exec(
                            ec.conn,
                            b"SELECT nextval('plex.statistics_media_id_seq')\0".as_ptr()
                                as *const c_char,
                        );
                        if crate::libpq_helpers::rust_pq_result_status(seq_res) == PGRES_TUPLES_OK
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
                            log_debug_lazy!(
                                "SKIP: Advanced sequence to {}",
                                cstr_to_string_or(seq_val, "?")
                            );
                        }
                        crate::libpq_helpers::rust_pq_clear(seq_res);
                    }
                }

                stmt.write_executed = 1;
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
                stmt.pg_sql,
                lib_col.as_ptr(),
            );
            let type_idx = crate::db_interpose_helpers::rust_find_insert_column_index(
                stmt.pg_sql,
                type_col.as_ptr(),
            );

            if lib_idx >= 0
                && type_idx >= 0
                && lib_idx < stmt.param_count
                && type_idx < stmt.param_count
            {
                let lib_val = param_at(param_values, lib_idx as usize);
                let type_val = param_at(param_values, type_idx as usize);
                if lib_val.is_null() && type_val.is_null() {
                    log_error(&format!(
                        "GUARD: Blocked junk INSERT into metadata_items (library_section_id=NULL, metadata_type=NULL) param_count={} lib_idx={} type_idx={}",
                        stmt.param_count, lib_idx, type_idx
                    ));

                    if !exec_conn.is_null() && !(&*exec_conn).conn.is_null() {
                        let ec2 = &mut *exec_conn;
                        let _conn_guard = PthreadMutexGuard::lock(&mut ec2.mutex as *mut _);
                        if !ec2.conn.is_null()
                            && crate::libpq_helpers::rust_pq_status(ec2.conn) == CONNECTION_OK
                        {
                            let seq_res = crate::libpq_helpers::rust_pq_exec(
                                ec2.conn,
                                b"SELECT nextval('plex.metadata_items_id_seq')\0".as_ptr()
                                    as *const c_char,
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
                                log_debug_lazy!(
                                    "GUARD: Advanced metadata_items sequence to {}",
                                    cstr_to_string_or(seq_val, "?")
                                );
                            }
                            crate::libpq_helpers::rust_pq_clear(seq_res);
                        }
                    }

                    stmt.write_executed = 1;
                    return 1;
                }
            }
        }

        0
    }
}
