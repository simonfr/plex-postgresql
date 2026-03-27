use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

const DECLTYPE_MAX_KEY_LEN: usize = 128;

static DECLTYPE_CACHE_LOADED: AtomicBool = AtomicBool::new(false);
static DECLTYPE_CACHE_MUTEX: Mutex<()> = Mutex::new(());

fn preload_decltype_cache(pg_conn: *mut PgConnection) {
    if DECLTYPE_CACHE_LOADED.load(Ordering::Acquire) {
        return;
    }
    if pg_conn.is_null() || unsafe { (*pg_conn).conn.is_null() } {
        return;
    }

    let _lock = match DECLTYPE_CACHE_MUTEX.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if DECLTYPE_CACHE_LOADED.load(Ordering::Acquire) {
        return;
    }

    log_info("DECLTYPE_CACHE: Loading column types from PostgreSQL information_schema...");

    let mut cache_conn = pg_conn;
    let streaming_active = unsafe { (*pg_conn).streaming_active.load(Ordering::SeqCst) != 0 };
    if streaming_active {
        log_debug_lazy!(
            "DECLTYPE_CACHE: Connection {:p} is streaming_active, getting alternate",
            pg_conn
        );
        let alt = unsafe {
            crate::pg_client::pg_get_thread_connection_excluding(
                (*pg_conn).db_path.as_ptr(),
                pg_conn as *const c_void,
            )
        };
        if !alt.is_null()
            && unsafe { !(*alt).conn.is_null() }
            && alt != pg_conn
            && unsafe { (*alt).streaming_active.load(Ordering::SeqCst) == 0 }
        {
            cache_conn = alt;
            log_debug_lazy!(
                "DECLTYPE_CACHE: Using alternate connection {:p}",
                cache_conn
            );
        } else {
            log_error("DECLTYPE_CACHE: No alternate connection available, deferring load");
            return;
        }
    }

    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut (*cache_conn).mutex as *mut _) };
    let res = unsafe {
        crate::libpq_helpers::rust_pq_exec(
            (*cache_conn).conn,
            b"SELECT table_name, column_name, udt_name FROM information_schema.columns WHERE table_schema = 'plex' AND table_name NOT IN ('sqlite_column_types', 'sqlite_sequence') ORDER BY table_name, ordinal_position\0"
                .as_ptr() as *const c_char,
        )
    };

    if res.is_null() || crate::libpq_helpers::rust_pq_result_status(res) != PGRES_TUPLES_OK {
        log_error(&format!(
            "DECLTYPE_CACHE: Failed to query information_schema: {}",
            if res.is_null() {
                "NULL result".to_string()
            } else {
                cstr_to_string_or(
                    unsafe { crate::libpq_helpers::rust_pq_error_message((*cache_conn).conn) },
                    "?",
                )
            }
        ));
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        DECLTYPE_CACHE_LOADED.store(true, Ordering::Release);
        return;
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
    DECLTYPE_CACHE_LOADED.store(true, Ordering::Release);

    log_info_lazy!(
        "DECLTYPE_CACHE: Loaded {} column types from PG ({} skipped)",
        loaded, skipped
    );
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
