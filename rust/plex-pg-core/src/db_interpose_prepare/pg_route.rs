use super::*;

fn should_route_via_pg(pg_conn: *mut PgConnection, is_read: bool, is_write: bool) -> bool {
    !pg_conn.is_null()
        && unsafe { (*pg_conn).is_pg_active } != 0
        && unsafe { !(*pg_conn).conn.is_null() }
        && (is_read || is_write)
        && crate::db_interpose_helpers::rust_is_library_db_path(unsafe {
            (*pg_conn).db_path.as_ptr()
        }) != 0
}

pub(super) unsafe fn should_use_dummy_shadow(
    pg_conn: *mut PgConnection,
    z_sql: *const c_char,
    is_read: bool,
    is_write: bool,
) -> bool {
    should_route_via_pg(pg_conn, is_read, is_write)
        && crate::pg_config::pg_config_should_skip_sql(z_sql) == 0
}

unsafe fn translated_sql_for_pg_stmt(
    pg_conn: *mut PgConnection,
    z_sql: *const c_char,
    trans: &SqlTranslation,
) -> *mut c_char {
    let blobs_rewrite = rewrite_blobs_schema_migrations(trans.sql, (*pg_conn).db_path.as_ptr());
    let effective_sql = if blobs_rewrite.is_null() {
        trans.sql
    } else {
        blobs_rewrite
    };

    let aliased = crate::db_interpose_prepare_helpers::alias_collection_sync_aggregates(
        &cstr_to_string_or(z_sql, ""),
        &cstr_to_string_or(effective_sql, ""),
    );
    let pg_sql_ptr = if let Some(s) = aliased {
        let cs = CString::new(s).ok();
        if let Some(cs) = cs.as_ref() {
            libc::strdup(cs.as_ptr())
        } else {
            libc::strdup(effective_sql)
        }
    } else {
        libc::strdup(effective_sql)
    };

    if !blobs_rewrite.is_null() {
        libc::free(blobs_rewrite as *mut c_void);
    }

    pg_sql_ptr
}

unsafe fn maybe_mark_count_query(pg_stmt: *mut PgStmt) {
    if (*pg_stmt).pg_sql.is_null() {
        return;
    }
    if contains_ascii_icase(
        CStr::from_ptr((*pg_stmt).pg_sql).to_bytes(),
        b"parents.parent_id,count(*)",
    ) {
        (*pg_stmt).is_count_query = 1;
    }
}

unsafe fn replace_stmt_sql_with_suffix(
    pg_stmt: *mut PgStmt,
    suffix: *const c_char,
    extra_bytes: usize,
    log_label: Option<&str>,
) {
    let len = libc::strlen((*pg_stmt).pg_sql);
    let replaced = libc::malloc(len + extra_bytes) as *mut c_char;
    if replaced.is_null() {
        return;
    }

    libc::snprintf(
        replaced,
        len + extra_bytes,
        b"%s %s\0".as_ptr() as *const c_char,
        (*pg_stmt).pg_sql,
        suffix,
    );
    if let Some(label) = log_label {
        log_info(&format!(
            "{}: {}",
            label,
            cstr_prefix(replaced, 200, "NULL")
        ));
    }
    libc::free((*pg_stmt).pg_sql as *mut c_void);
    (*pg_stmt).pg_sql = replaced;
}

unsafe fn maybe_adjust_insert_sql(pg_stmt: *mut PgStmt, bytes: &[u8], is_write: bool) {
    if !is_write || !starts_with_ascii_icase(bytes, b"INSERT") || (*pg_stmt).pg_sql.is_null() {
        return;
    }

    if contains_icase_ptr((*pg_stmt).pg_sql, "schema_migrations")
        && !contains_icase_ptr((*pg_stmt).pg_sql, "ON CONFLICT")
    {
        replace_stmt_sql_with_suffix(
            pg_stmt,
            c"ON CONFLICT DO NOTHING".as_ptr(),
            40,
            Some("SCHEMA_MIGRATIONS: Added ON CONFLICT DO NOTHING"),
        );
        return;
    }

    if !contains_icase_ptr((*pg_stmt).pg_sql, "RETURNING") {
        let label = if contains_icase_ptr((*pg_stmt).pg_sql, "play_queue_generators") {
            Some("PREPARE play_queue_generators INSERT with RETURNING")
        } else {
            None
        };
        replace_stmt_sql_with_suffix(pg_stmt, c"RETURNING id".as_ptr(), 20, label);
    }
}

pub(super) unsafe fn maybe_register_pg_stmt(
    pg_conn: *mut PgConnection,
    z_sql: *const c_char,
    bytes: &[u8],
    pp_stmt: *mut *mut sqlite3_stmt,
    is_read: bool,
    is_write: bool,
    pre_trans: &mut SqlTranslation,
    have_pre_trans: &mut bool,
) {
    if !should_route_via_pg(pg_conn, is_read, is_write) || pp_stmt.is_null() || (*pp_stmt).is_null()
    {
        return;
    }

    let pg_stmt = pg_stmt_create(pg_conn, z_sql, *pp_stmt);
    if pg_stmt.is_null() {
        return;
    }

    if crate::pg_config::pg_config_should_skip_sql(z_sql) != 0 {
        (*pg_stmt).is_pg = 3;
        pg_register_stmt(*pp_stmt, pg_stmt);
        return;
    }

    (*pg_stmt).is_pg = if is_write { 1 } else { 2 };

    let mut trans = if *have_pre_trans {
        *have_pre_trans = false;
        std::ptr::read(pre_trans)
    } else {
        sql_translate(z_sql)
    };

    if trans.success == 0 {
        log_error(&format!(
            "Translation failed for SQL: {}. Error: {}",
            cstr_prefix(z_sql, 200, "NULL"),
            cstr_to_string_or(trans.error.as_ptr(), "")
        ));
    }

    (*pg_stmt).param_count = trans.param_count;
    copy_param_names(pg_stmt, &trans);

    if trans.success != 0 && !trans.sql.is_null() {
        (*pg_stmt).pg_sql = translated_sql_for_pg_stmt(pg_conn, z_sql, &trans);
        trace_prepare_pgsql_if_enabled(z_sql, (*pg_stmt).pg_sql);
        maybe_mark_count_query(pg_stmt);
        maybe_adjust_insert_sql(pg_stmt, bytes, is_write);
        apply_prepared_stmt_settings(pg_stmt);
    }

    sql_translation_free(&mut trans as *mut SqlTranslation);
    pg_register_stmt(*pp_stmt, pg_stmt);
}
