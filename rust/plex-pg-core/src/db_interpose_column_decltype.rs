use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

const DECLTYPE_MAX_KEY_LEN: usize = 128;
const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;

static DECLTYPE_CACHE_LOADED: AtomicBool = AtomicBool::new(false);
static DECLTYPE_CACHE_MUTEX: Mutex<()> = Mutex::new(());

struct SqliteStmtGuard(*mut sqlite3_stmt);

impl Drop for SqliteStmtGuard {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        if let Some(finalize) = crate::db_interpose_common::get_orig_sqlite3_finalize() {
            unsafe {
                finalize(self.0);
            }
        }
    }
}

fn escape_sqlite_string(value: &str) -> String {
    value.replace('\'', "''")
}

unsafe fn sqlite_column_string(stmt: *mut sqlite3_stmt, idx: c_int) -> Option<String> {
    let column_text = crate::db_interpose_common::get_orig_sqlite3_column_text()?;
    let text = column_text(stmt, idx) as *const c_char;
    if text.is_null() {
        return None;
    }
    Some(CStr::from_ptr(text).to_string_lossy().into_owned())
}

unsafe fn sqlite_prepare_stmt(db: *mut sqlite3, sql: &CString) -> Option<*mut sqlite3_stmt> {
    let prepare = crate::db_interpose_common::get_orig_sqlite3_prepare_v2()?;
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    let rc = prepare(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
    if rc != SQLITE_OK || stmt.is_null() {
        return None;
    }
    Some(stmt)
}

fn normalize_shadow_decltype(decltype: &str) -> &str {
    // Plex's bundled SOCI recognizes 64-bit SQLite integers by finding the
    // substring "integer(8)" in the declared type. Shadow SQLite can expose
    // migrated "datetime" decltypes for epoch-backed columns, which makes SOCI
    // allocate a date/std::tm holder instead. Canonicalize those back to the
    // original 64-bit integer form before caching.
    if decltype.eq_ignore_ascii_case("datetime") {
        "dt_integer(8)"
    } else {
        decltype
    }
}

unsafe fn preload_shadow_table_decltypes(
    shadow_db: *mut sqlite3,
    table_name: &str,
) -> (usize, usize) {
    let pragma_sql = match CString::new(format!(
        "PRAGMA table_info('{}')",
        escape_sqlite_string(table_name)
    )) {
        Ok(sql) => sql,
        Err(_) => return (0, 1),
    };
    let Some(stmt) = sqlite_prepare_stmt(shadow_db, &pragma_sql) else {
        return (0, 1);
    };
    let _stmt_guard = SqliteStmtGuard(stmt);
    let Some(step) = crate::db_interpose_common::get_orig_sqlite3_step() else {
        return (0, 1);
    };

    let mut loaded = 0usize;
    let mut skipped = 0usize;
    loop {
        let rc = step(stmt);
        if rc != SQLITE_ROW {
            break;
        }

        let Some(column_name) = sqlite_column_string(stmt, 1) else {
            skipped += 1;
            continue;
        };
        let Some(decltype) = sqlite_column_string(stmt, 2) else {
            skipped += 1;
            continue;
        };
        if column_name.is_empty() || decltype.is_empty() {
            skipped += 1;
            continue;
        }

        let key = match CString::new(format!("{}_{}", table_name, column_name)) {
            Ok(key) => key,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let decltype = match CString::new(normalize_shadow_decltype(&decltype)) {
            Ok(decl) => decl,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        if crate::db_interpose_helpers::rust_decltype_cache_insert(key.as_ptr(), decltype.as_ptr())
            != 0
        {
            loaded += 1;
        } else {
            skipped += 1;
        }
    }

    (loaded, skipped)
}

unsafe fn resolve_shadow_db_for_decltype_preload(pg_conn: *mut PgConnection) -> *mut sqlite3 {
    if pg_conn.is_null() {
        return ptr::null_mut();
    }

    let shadow_db = (*pg_conn).shadow_db;
    if !shadow_db.is_null() {
        return shadow_db;
    }

    let db_path =
        crate::db_interpose_helpers::cstr_to_str_or_empty((*pg_conn).db_path.as_ptr()).to_owned();

    let fallback_conn = crate::pg_client::pool()
        .registry
        .find_any(|conn_ptr| {
            let conn = conn_ptr as *mut PgConnection;
            if conn.is_null() {
                return false;
            }

            let conn_ref = unsafe { &*conn };
            if conn_ref.shadow_db.is_null() || conn_ref.is_pg_active == 0 {
                return false;
            }

            db_path.is_empty()
                || unsafe {
                    crate::db_interpose_helpers::cstr_to_str_or_empty(conn_ref.db_path.as_ptr())
                        == db_path.as_str()
                }
        })
        .map(|conn_ptr| conn_ptr as *mut PgConnection)
        .unwrap_or(ptr::null_mut());

    if fallback_conn.is_null() {
        return ptr::null_mut();
    }

    log_debug_lazy!(
        "DECLTYPE_CACHE: Using registered shadow_db from connection {:p} for preload on {:p}",
        fallback_conn,
        pg_conn
    );
    (*fallback_conn).shadow_db
}

unsafe fn preload_decltype_cache_from_shadow(pg_conn: *mut PgConnection) -> Option<(usize, usize)> {
    if pg_conn.is_null() {
        return None;
    }
    let shadow_db = resolve_shadow_db_for_decltype_preload(pg_conn);
    if shadow_db.is_null() {
        return None;
    }

    let list_sql = CString::new(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .ok()?;
    let stmt = sqlite_prepare_stmt(shadow_db, &list_sql)?;
    let _stmt_guard = SqliteStmtGuard(stmt);
    let step = crate::db_interpose_common::get_orig_sqlite3_step()?;

    let mut loaded = 0usize;
    let mut skipped = 0usize;
    loop {
        let rc = step(stmt);
        if rc != SQLITE_ROW {
            break;
        }
        let Some(table_name) = sqlite_column_string(stmt, 0) else {
            skipped += 1;
            continue;
        };

        let (table_loaded, table_skipped) = preload_shadow_table_decltypes(shadow_db, &table_name);
        loaded += table_loaded;
        skipped += table_skipped;
    }

    Some((loaded, skipped))
}

fn preload_decltype_cache_from_pg(pg_conn: *mut PgConnection) -> Option<(usize, usize)> {
    if pg_conn.is_null() {
        return None;
    }
    let pc = unsafe { &*pg_conn };
    if pc.conn.is_null() {
        return None;
    }

    let mut cache_conn = pg_conn;
    let streaming_active = pc.streaming_active.load(Ordering::SeqCst) != 0;
    if streaming_active {
        log_debug_lazy!(
            "DECLTYPE_CACHE: Connection {:p} is streaming_active, getting alternate",
            pg_conn
        );
        let alt = crate::pg_client::pg_get_thread_connection_excluding(
            pc.db_path.as_ptr(),
            pg_conn as *const c_void,
        );
        if !alt.is_null()
            && unsafe { !(&*alt).conn.is_null() }
            && alt != pg_conn
            && unsafe { (&*alt).streaming_active.load(Ordering::SeqCst) == 0 }
        {
            cache_conn = alt;
            log_debug_lazy!(
                "DECLTYPE_CACHE: Using alternate connection {:p}",
                cache_conn
            );
        } else {
            log_error("DECLTYPE_CACHE: No alternate connection available, deferring load");
            return None;
        }
    }

    let cc = unsafe { &mut *cache_conn };
    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut cc.mutex as *mut _) };
    let res = crate::libpq_helpers::rust_pq_exec(
        cc.conn,
        b"SELECT table_name, column_name, udt_name FROM information_schema.columns WHERE table_schema = 'plex' AND table_name NOT IN ('sqlite_column_types', 'sqlite_sequence') ORDER BY table_name, ordinal_position\0"
            .as_ptr() as *const c_char,
    );

    if res.is_null() || crate::libpq_helpers::rust_pq_result_status(res) != PGRES_TUPLES_OK {
        log_error(&format!(
            "DECLTYPE_CACHE: Failed to query information_schema: {}",
            if res.is_null() {
                "NULL result".to_string()
            } else {
                cstr_to_string_or(crate::libpq_helpers::rust_pq_error_message(cc.conn), "?")
            }
        ));
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        return None;
    }

    let num_rows = crate::libpq_helpers::rust_pq_ntuples(res);
    let mut loaded = 0;
    let mut skipped = 0;

    for i in 0..num_rows {
        let mut table_buf = [0 as c_char; 128];
        let mut column_buf = [0 as c_char; 128];
        let mut udt_buf = [0 as c_char; 128];
        let table_ok = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(res),
            i,
            0,
            table_buf.as_mut_ptr(),
            table_buf.len(),
        );
        if table_ok < 0 {
            continue;
        }
        let col_ok = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(res),
            i,
            1,
            column_buf.as_mut_ptr(),
            column_buf.len(),
        );
        if col_ok < 0 {
            continue;
        }
        let udt_ok = crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(res),
            i,
            2,
            udt_buf.as_mut_ptr(),
            udt_buf.len(),
        );
        if udt_ok < 0 {
            continue;
        }

        let table = table_buf.as_ptr();
        let column = column_buf.as_ptr();
        let udt_name = udt_buf.as_ptr();

        let sqlite_type = crate::db_interpose_helpers::rust_pg_udt_to_sqlite_decltype(udt_name);

        let table_str = unsafe { CStr::from_ptr(table) }.to_string_lossy();
        let col_str = unsafe { CStr::from_ptr(column) }.to_string_lossy();
        let mut key = String::with_capacity(DECLTYPE_MAX_KEY_LEN);
        key.push_str(&table_str);
        key.push('_');
        key.push_str(&col_str);
        let key_cs = match CString::new(key) {
            Ok(cs) => cs,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let inserted =
            crate::db_interpose_helpers::rust_decltype_cache_insert(key_cs.as_ptr(), sqlite_type);
        if inserted != 0 {
            loaded += 1;
        } else {
            skipped += 1;
        }
    }

    crate::libpq_helpers::rust_pq_clear(res);
    Some((loaded, skipped))
}

fn preload_decltype_cache(pg_conn: *mut PgConnection) {
    if DECLTYPE_CACHE_LOADED.load(Ordering::Acquire) || pg_conn.is_null() {
        return;
    }

    let _lock = match DECLTYPE_CACHE_MUTEX.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if DECLTYPE_CACHE_LOADED.load(Ordering::Acquire) {
        return;
    }

    if let Some((loaded, skipped)) = unsafe { preload_decltype_cache_from_shadow(pg_conn) } {
        if loaded > 0 {
            DECLTYPE_CACHE_LOADED.store(true, Ordering::Release);
            log_info_lazy!(
                "DECLTYPE_CACHE: Loaded {} original decltypes from shadow SQLite ({} skipped)",
                loaded,
                skipped
            );
            return;
        }

        log_info_lazy!(
            "DECLTYPE_CACHE: Shadow SQLite returned 0 decltypes ({} skipped), falling back to PostgreSQL information_schema",
            skipped
        );
    }

    log_info("DECLTYPE_CACHE: Shadow SQLite decltypes unavailable, falling back to PostgreSQL information_schema");
    if let Some((loaded, skipped)) = preload_decltype_cache_from_pg(pg_conn) {
        DECLTYPE_CACHE_LOADED.store(true, Ordering::Release);
        log_info_lazy!(
            "DECLTYPE_CACHE: Loaded {} fallback decltypes from PG ({} skipped)",
            loaded,
            skipped
        );
    }
}

pub(super) fn lookup_decltype_direct(pg_conn: *mut PgConnection, cache_key: &str) -> *const c_char {
    if cache_key.is_empty() {
        return ptr::null();
    }
    if !DECLTYPE_CACHE_LOADED.load(Ordering::Acquire) && !pg_conn.is_null() {
        preload_decltype_cache(pg_conn);
    }
    let key_cs = match CString::new(cache_key) {
        Ok(cs) => cs,
        Err(_) => return ptr::null(),
    };
    let cached = crate::db_interpose_helpers::rust_decltype_cache_lookup(key_cs.as_ptr());
    if !cached.is_null() {
        log_debug_lazy!(
            "DECLTYPE_DIRECT: found '{}' -> '{}'",
            cache_key,
            cstr_to_string_or(cached, "?")
        );
        return cached;
    }
    log_debug_lazy!("DECLTYPE_DIRECT: '{}' not in cache", cache_key);
    ptr::null()
}

pub(super) fn lookup_sqlite_decltype(
    pg_conn: *mut PgConnection,
    col_alias: *const c_char,
) -> *const c_char {
    if col_alias.is_null() || unsafe { *col_alias == 0 } {
        return ptr::null();
    }
    if !DECLTYPE_CACHE_LOADED.load(Ordering::Acquire) && !pg_conn.is_null() {
        preload_decltype_cache(pg_conn);
    }
    let cached = crate::db_interpose_helpers::rust_decltype_cache_lookup_alias(col_alias);
    if cached.is_null() {
        log_debug_lazy!(
            "DECLTYPE_LOOKUP: no match for '{}'",
            cstr_to_string_or(col_alias, "?")
        );
    }
    cached
}

#[cfg(test)]
mod tests {
    use super::normalize_shadow_decltype;

    #[test]
    fn shadow_datetime_decltype_is_canonicalized_to_int64() {
        assert_eq!(normalize_shadow_decltype("datetime"), "dt_integer(8)");
        assert_eq!(normalize_shadow_decltype("DATETIME"), "dt_integer(8)");
    }

    #[test]
    fn shadow_non_datetime_decltype_is_preserved() {
        assert_eq!(normalize_shadow_decltype("dt_integer(8)"), "dt_integer(8)");
        assert_eq!(normalize_shadow_decltype("INTEGER(8)"), "INTEGER(8)");
        assert_eq!(normalize_shadow_decltype("varchar(255)"), "varchar(255)");
    }
}
