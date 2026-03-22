use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uchar, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

use crate::db_interpose_common::{tls_column_type_calls_ptr, tls_in_resolve_tables_ptr, tls_last_query_ptr};
use crate::db_interpose_conn_utils::{cstr_prefix, cstr_to_string_or, log_debug, log_error, log_info, PthreadMutexGuard};
use crate::db_interpose_trace_helpers::{list_any_token_in_haystack, list_contains_idx};
use crate::db_interpose_value_helpers::{
    pg_oid_to_sqlite_type_impl, pg_text_to_double_impl, pg_text_to_int64_impl, pg_text_to_int_impl,
};
use crate::db_interpose_helpers::PGresult as PgResultHelpers;
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgConnection, PgStmt, MAX_PARAMS};
use crate::libpq_helpers::PGresult as PgResultLibpq;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;

const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;

const DECLTYPE_MAX_KEY_LEN: usize = 128;
const NUM_TEXT_BUFFERS: usize = 64;
const TEXT_BUFFER_SIZE: usize = 8192;

const INVALID_OID: u32 = 0;
const PG_DECLTYPE_CASE_NULL: c_int = 1;
const PG_DECLTYPE_CASE_DT_INTEGER_8: c_int = 2;

const MAX_FAKE_VALUES: usize = 4096;
const PG_FAKE_VALUE_MAGIC: u32 = 0x50475641;

const PMT_COLUMN_CACHED_BLOB_ALLOC: c_int = 3;
const PMT_COLUMN_DECODED_BLOB_ALLOC: c_int = 4;

static DECLTYPE_TEXT: &[u8] = b"TEXT\0";
static DECLTYPE_DT_INTEGER_8: &[u8] = b"dt_integer(8)\0";
static DECLTYPE_INTEGER: &[u8] = b"INTEGER\0";
static DECLTYPE_BIGINT: &[u8] = b"BIGINT\0";
static TRACE_DEFAULT_IDX: &[u8] = b"5,6\0";
static TRACE_DEFAULT_THREAD: &[u8] = b"any\0";
static NEEDLE_TYPE: &[u8] = b"type\0";
static NEEDLE_METADATA_TYPE: &[u8] = b"metadata_type\0";

static DECLTYPE_CACHE_LOADED: AtomicBool = AtomicBool::new(false);
static DECLTYPE_CACHE_MUTEX: Mutex<()> = Mutex::new(());

thread_local! {
    static COLUMN_TEXT_BUFFERS: RefCell<[[u8; TEXT_BUFFER_SIZE]; NUM_TEXT_BUFFERS]> =
        RefCell::new([[0u8; TEXT_BUFFER_SIZE]; NUM_TEXT_BUFFERS]);
    static COLUMN_TEXT_BUF_IDX: Cell<usize> = Cell::new(0);
}

static TRACE_BADCAST_INIT: Once = Once::new();
static mut TRACE_BADCAST_ENABLED: c_int = -1;
const TRACE_IDX_BUF_LEN: usize = 256;
const TRACE_SQL_BUF_LEN: usize = 256;
const TRACE_THREAD_BUF_LEN: usize = 128;
const TRACE_COL_BUF_LEN: usize = 128;
static mut TRACE_BADCAST_IDX_BUF: [c_char; TRACE_IDX_BUF_LEN] = [0; TRACE_IDX_BUF_LEN];
static mut TRACE_BADCAST_SQL_BUF: [c_char; TRACE_SQL_BUF_LEN] = [0; TRACE_SQL_BUF_LEN];
static mut TRACE_BADCAST_THREAD_BUF: [c_char; TRACE_THREAD_BUF_LEN] = [0; TRACE_THREAD_BUF_LEN];
static mut TRACE_BADCAST_COL_BUF: [c_char; TRACE_COL_BUF_LEN] = [0; TRACE_COL_BUF_LEN];

static mut TRACE_BADCAST_IDX_LIST: *const c_char = ptr::null();
static mut TRACE_BADCAST_THREAD_SUBSTR: *const c_char = ptr::null();
static mut TRACE_BADCAST_SQL_CONTAINS: *const c_char = ptr::null();
static mut TRACE_BADCAST_COL_CONTAINS: *const c_char = ptr::null();

#[repr(C)]
struct PgFakeValue {
    magic: u32,
    pg_stmt: *mut c_void,
    col_idx: c_int,
    row_idx: c_int,
    owner_thread: libc::pthread_t,
}

extern "C" {
    static mut orig_sqlite3_column_count: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_column_type: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int>;
    static mut orig_sqlite3_column_int: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int>;
    static mut orig_sqlite3_column_int64: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> i64>;
    static mut orig_sqlite3_column_double: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> f64>;
    static mut orig_sqlite3_column_text: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_uchar>;
    static mut orig_sqlite3_column_blob: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_void>;
    static mut orig_sqlite3_column_bytes: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int>;
    static mut orig_sqlite3_column_name: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_char>;
    static mut orig_sqlite3_column_decltype: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_char>;
    static mut orig_sqlite3_column_value: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *mut sqlite3_value>;
    static mut orig_sqlite3_data_count: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_db_handle: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut sqlite3>;

    static mut last_query_being_processed: *const c_char;
    static mut last_column_being_accessed: *const c_char;
    static mut global_column_type_calls: c_long;

    static mut fake_value_pool: [PgFakeValue; MAX_FAKE_VALUES];
    static mut fake_value_next: c_uint;
    static mut fake_value_mutex: libc::pthread_mutex_t;

    fn pg_find_any_stmt(stmt: *mut sqlite3_stmt) -> *mut PgStmt;
    fn pg_get_thread_connection(db_path: *const c_char) -> *mut PgConnection;
    fn pg_stmt_cache_lookup(conn: *mut PgConnection, sql_hash: u64, stmt_name_out: *mut *const c_char) -> c_int;
    fn pg_stmt_cache_add(conn: *mut PgConnection, sql_hash: u64, stmt_name: *const c_char, param_count: c_int) -> c_int;
    fn pg_is_duplicate_prepared_stmt(res: *mut PgResultLibpq) -> c_int;

    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
}

#[inline]
fn helpers_result_ptr(result: *mut PgResultLibpq) -> *const PgResultHelpers {
    result as *const PgResultHelpers
}

fn sqlite_type_name(t: c_int) -> &'static str {
    match t {
        SQLITE_INTEGER => "INTEGER",
        SQLITE_FLOAT => "FLOAT",
        SQLITE_TEXT => "TEXT",
        SQLITE_BLOB => "BLOB",
        SQLITE_NULL => "NULL",
        _ => "UNKNOWN",
    }
}

fn next_text_buffer_index() -> usize {
    COLUMN_TEXT_BUF_IDX.with(|idx| {
        let cur = idx.get();
        idx.set((cur + 1) % NUM_TEXT_BUFFERS);
        cur
    })
}

fn trace_badcast_init() {
    TRACE_BADCAST_INIT.call_once(|| unsafe {
        let enabled_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_ENABLED);
        let idx_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_IDX_BUF) as *mut c_char;
        let thread_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_THREAD_BUF) as *mut c_char;
        let sql_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_SQL_BUF) as *mut c_char;
        let col_ptr = std::ptr::addr_of_mut!(TRACE_BADCAST_COL_BUF) as *mut c_char;

        crate::db_interpose_helpers::rust_load_badcast_config(
            enabled_ptr,
            idx_ptr,
            TRACE_IDX_BUF_LEN,
            thread_ptr,
            TRACE_THREAD_BUF_LEN,
            sql_ptr,
            TRACE_SQL_BUF_LEN,
            col_ptr,
            TRACE_COL_BUF_LEN,
        );

        let idx_first = *std::ptr::addr_of!(TRACE_BADCAST_IDX_BUF).cast::<c_char>();
        TRACE_BADCAST_IDX_LIST = if idx_first != 0 {
            std::ptr::addr_of!(TRACE_BADCAST_IDX_BUF) as *const c_char
        } else {
            TRACE_DEFAULT_IDX.as_ptr() as *const c_char
        };

        let thread_first = *std::ptr::addr_of!(TRACE_BADCAST_THREAD_BUF).cast::<c_char>();
        TRACE_BADCAST_THREAD_SUBSTR = if thread_first != 0 {
            std::ptr::addr_of!(TRACE_BADCAST_THREAD_BUF) as *const c_char
        } else {
            TRACE_DEFAULT_THREAD.as_ptr() as *const c_char
        };

        let sql_first = *std::ptr::addr_of!(TRACE_BADCAST_SQL_BUF).cast::<c_char>();
        TRACE_BADCAST_SQL_CONTAINS = if sql_first != 0 {
            std::ptr::addr_of!(TRACE_BADCAST_SQL_BUF) as *const c_char
        } else {
            ptr::null()
        };

        let col_first = *std::ptr::addr_of!(TRACE_BADCAST_COL_BUF).cast::<c_char>();
        TRACE_BADCAST_COL_CONTAINS = if col_first != 0 {
            std::ptr::addr_of!(TRACE_BADCAST_COL_BUF) as *const c_char
        } else {
            ptr::null()
        };
    });
}

fn trace_badcast_enabled() -> bool {
    trace_badcast_init();
    unsafe { TRACE_BADCAST_ENABLED != 0 }
}

fn trace_badcast_thread_ok() -> bool {
    trace_badcast_init();
    if !trace_badcast_enabled() {
        return false;
    }
    let substr_ptr = unsafe { TRACE_BADCAST_THREAD_SUBSTR };
    if substr_ptr.is_null() {
        return true;
    }
    let substr = unsafe { CStr::from_ptr(substr_ptr) }.to_string_lossy();
    if substr.is_empty() {
        return true;
    }
    if substr.eq_ignore_ascii_case("any") {
        return true;
    }

    #[cfg(target_os = "macos")]
    unsafe {
        let mut name_buf = [0u8; 64];
        let rc = libc::pthread_getname_np(libc::pthread_self(), name_buf.as_mut_ptr() as *mut c_char, name_buf.len());
        if rc != 0 {
            return false;
        }
        let tname = CStr::from_ptr(name_buf.as_ptr() as *const c_char).to_string_lossy();
        if tname.is_empty() {
            return false;
        }
        return tname.contains(substr.as_ref());
    }

    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

fn trace_badcast_sql_ok(pg_stmt: *const PgStmt) -> bool {
    trace_badcast_init();
    if !trace_badcast_enabled() {
        return false;
    }
    let sql_filter = unsafe { TRACE_BADCAST_SQL_CONTAINS };
    if sql_filter.is_null() || unsafe { *sql_filter == 0 } {
        return true;
    }
    if pg_stmt.is_null() {
        return false;
    }
    let sql_ptr = unsafe { (*pg_stmt).pg_sql };
    if sql_ptr.is_null() {
        return false;
    }
    let list = unsafe { CStr::from_ptr(sql_filter) }.to_string_lossy();
    let haystack = unsafe { CStr::from_ptr(sql_ptr) }.to_string_lossy();
    list_any_token_in_haystack(list.as_ref(), haystack.as_ref())
}

fn trace_badcast_col_ok(col_name: *const c_char) -> bool {
    trace_badcast_init();
    if !trace_badcast_enabled() {
        return false;
    }
    let col_filter = unsafe { TRACE_BADCAST_COL_CONTAINS };
    if col_filter.is_null() || unsafe { *col_filter == 0 } {
        return true;
    }
    if col_name.is_null() {
        return false;
    }
    let list = unsafe { CStr::from_ptr(col_filter) }.to_string_lossy();
    let haystack = unsafe { CStr::from_ptr(col_name) }.to_string_lossy();
    list_any_token_in_haystack(list.as_ref(), haystack.as_ref())
}

fn trace_badcast_list_contains_idx(idx: c_int) -> bool {
    trace_badcast_init();
    let list_ptr = unsafe { TRACE_BADCAST_IDX_LIST };
    if list_ptr.is_null() {
        return false;
    }
    let list = unsafe { CStr::from_ptr(list_ptr) }.to_string_lossy();
    list_contains_idx(list.as_ref(), idx)
}

fn trace_badcast_should_log(pg_stmt: *const PgStmt, idx: c_int) -> bool {
    trace_badcast_init();
    if !trace_badcast_enabled() {
        return false;
    }
    if !trace_badcast_thread_ok() {
        return false;
    }
    if !trace_badcast_sql_ok(pg_stmt) {
        return false;
    }
    trace_badcast_list_contains_idx(idx)
}

fn trace_badcast_should_log_col(pg_stmt: *const PgStmt, idx: c_int, col_name: *const c_char) -> bool {
    if !trace_badcast_should_log(pg_stmt, idx) {
        return false;
    }
    trace_badcast_col_ok(col_name)
}

fn trace_badcast_log_ctx(
    pg_stmt: *const PgStmt,
    stmt: *const sqlite3_stmt,
    idx: c_int,
    fn_name: &str,
    phase: &str,
    row: c_int,
    is_null: c_int,
    oid: u32,
    col_name: *const c_char,
) {
    if pg_stmt.is_null() {
        return;
    }
    let sql_ptr = unsafe { (*pg_stmt).pg_sql };
    let sql = cstr_prefix(sql_ptr, 200, "?");
    let col = cstr_to_string_or(col_name, "?");
    let num_rows = unsafe { (*pg_stmt).num_rows };
    let num_cols = unsafe { (*pg_stmt).num_cols };
    log_debug(&format!(
        "TRACE_BADCAST_CTX: fn={} phase={} stmt={:p} pg_stmt={:p} idx={} col='{}' oid={} row={}/{} cols={} is_null={} sql={}",
        fn_name,
        phase,
        stmt,
        pg_stmt,
        idx,
        col,
        oid,
        row,
        num_rows,
        num_cols,
        is_null,
        sql
    ));
}

fn mask_collection_metadata_type(
    pg_stmt: *const PgStmt,
    col_name: *const c_char,
    raw_val: i64,
    out: &mut i64,
) -> bool {
    if pg_stmt.is_null() || col_name.is_null() {
        return false;
    }
    let sql_ptr = unsafe { (*pg_stmt).pg_sql };
    if sql_ptr.is_null() {
        return false;
    }
    let rc = crate::db_interpose_helpers::rust_should_mask_collection_metadata_type(sql_ptr, col_name, raw_val);
    if rc == 0 {
        return false;
    }
    let row = unsafe { (*pg_stmt).current_row };
    log_debug(&format!(
        "COMPAT_TYPE18: masking metadata_type 18 -> 0 for related-items query, row {}",
        row
    ));
    *out = 0;
    true
}

fn validate_type_consistency(p_stmt: *mut sqlite3_stmt, idx: c_int, accessor_name: &str) {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    if pg_stmt.is_null() || unsafe { (*pg_stmt).is_pg == 0 } {
        return;
    }

    let col_type = rust_my_sqlite3_column_type(p_stmt, idx);
    let col_decltype = rust_my_sqlite3_column_decltype(p_stmt, idx);

    let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };
    if unsafe { (*pg_stmt).result.is_null() } {
        return;
    }

    let oid = unsafe {
        crate::db_interpose_helpers::rust_pg_result_col_oid(helpers_result_ptr((*pg_stmt).result), idx)
    };
    let col_name = unsafe {
        crate::db_interpose_helpers::rust_pg_result_col_name(helpers_result_ptr((*pg_stmt).result), idx)
    };

    if !col_decltype.is_null() {
        let expected = crate::db_interpose_helpers::rust_expected_sqlite_type_for_decltype(col_decltype);
        if expected != -1 && col_type != SQLITE_NULL && col_type != expected {
            log_debug(&format!(
                "TYPE_MISMATCH: accessor={} col='{}' idx={} decltype='{}' expects {} but column_type returned {} (OID={})",
                accessor_name,
                cstr_to_string_or(col_name, "?"),
                idx,
                cstr_to_string_or(col_decltype, "?"),
                sqlite_type_name(expected),
                sqlite_type_name(col_type),
                oid
            ));

            if trace_badcast_should_log(pg_stmt, idx) {
                trace_badcast_log_ctx(
                    pg_stmt,
                    p_stmt,
                    idx,
                    accessor_name,
                    "type_mismatch",
                    unsafe { (*pg_stmt).current_row },
                    if col_type == SQLITE_NULL { 1 } else { 0 },
                    oid,
                    col_name,
                );
                log_debug(&format!(
                    "TRACE_BADCAST_MISMATCH: accessor={} col='{}' idx={} oid={} decltype='{}' expected={} actual={} sql={}",
                    accessor_name,
                    cstr_to_string_or(col_name, "?"),
                    idx,
                    oid,
                    cstr_to_string_or(col_decltype, "?"),
                    sqlite_type_name(expected),
                    sqlite_type_name(col_type),
                    cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                ));
            }
        }
    }
}

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
        log_debug(&format!(
            "DECLTYPE_CACHE: Connection {:p} is streaming_active, getting alternate",
            pg_conn
        ));
        let alt = unsafe { pg_get_thread_connection((*pg_conn).db_path.as_ptr()) };
        if !alt.is_null()
            && unsafe { !(*alt).conn.is_null() }
            && alt != pg_conn
            && unsafe { (*alt).streaming_active.load(Ordering::SeqCst) == 0 }
        {
            cache_conn = alt;
            log_debug(&format!("DECLTYPE_CACHE: Using alternate connection {:p}", cache_conn));
        } else {
            log_error("DECLTYPE_CACHE: No alternate connection available, deferring load");
            return;
        }
    }

    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut (*cache_conn).mutex as *mut _) };
    let res = unsafe {
        crate::libpq_helpers::rust_pq_exec(
            (*cache_conn).conn,
            b"SELECT table_name, column_name, udt_name FROM information_schema.columns WHERE table_schema = 'plex' AND table_name NOT IN ('sqlite_column_types', 'sqlite_sequence') ORDER BY table_name, ordinal_position\0".as_ptr() as *const c_char,
        ) as *mut PgResultLibpq
    };

    if res.is_null() || crate::libpq_helpers::rust_pq_result_status(res) != PGRES_TUPLES_OK {
        log_error(&format!(
            "DECLTYPE_CACHE: Failed to query information_schema: {}",
            if res.is_null() {
                "NULL result".to_string()
            } else {
                cstr_to_string_or(unsafe { crate::libpq_helpers::rust_pq_error_message((*cache_conn).conn) }, "?")
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

        let inserted = crate::db_interpose_helpers::rust_decltype_cache_insert(key_cs.as_ptr(), sqlite_type);
        if inserted != 0 {
            loaded += 1;
        } else {
            skipped += 1;
        }
    }

    crate::libpq_helpers::rust_pq_clear(res);
    DECLTYPE_CACHE_LOADED.store(true, Ordering::Release);

    log_info(&format!(
        "DECLTYPE_CACHE: Loaded {} column types from PG ({} skipped)",
        loaded, skipped
    ));
}

fn lookup_decltype_direct(pg_conn: *mut PgConnection, cache_key: &str) -> *const c_char {
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
        log_debug(&format!(
            "DECLTYPE_DIRECT: found '{}' -> '{}'",
            cache_key,
            cstr_to_string_or(cached, "?")
        ));
        return cached;
    }
    log_debug(&format!("DECLTYPE_DIRECT: '{}' not in cache", cache_key));
    ptr::null()
}

fn lookup_sqlite_decltype(pg_conn: *mut PgConnection, col_alias: *const c_char) -> *const c_char {
    if col_alias.is_null() || unsafe { *col_alias == 0 } {
        return ptr::null();
    }
    if !DECLTYPE_CACHE_LOADED.load(Ordering::Acquire) && !pg_conn.is_null() {
        preload_decltype_cache(pg_conn);
    }
    let cached = crate::db_interpose_helpers::rust_decltype_cache_lookup_alias(col_alias);
    if cached.is_null() {
        log_debug(&format!(
            "DECLTYPE_LOOKUP: no match for '{}'",
            cstr_to_string_or(col_alias, "?")
        ));
    }
    cached
}

#[no_mangle]
pub extern "C" fn rust_resolve_column_tables(pg_stmt: *mut PgStmt, pg_conn: *mut PgConnection) -> c_int {
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
    if num_cols <= 0 || num_cols as usize > MAX_PARAMS {
        unsafe { (*pg_stmt).col_tables_resolved = 1 };
        return 0;
    }

    let mut table_oids = [0u32; MAX_PARAMS];
    let mut uncached_oids = [0usize; MAX_PARAMS];
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
        if !found && num_unique_tables < MAX_PARAMS {
            table_oids[num_unique_tables] = table_oid;
            uncached_oids[num_uncached] = num_unique_tables;
            num_unique_tables += 1;
            num_uncached += 1;
        }
    }

    if num_uncached == 0 {
        unsafe { (*pg_stmt).col_tables_resolved = 1 };
        if cache_hits > 0 {
            log_debug(&format!(
                "RESOLVE_TABLES: All {} columns resolved from cache (0 queries)",
                cache_hits
            ));
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
        log_debug(&format!(
            "RESOLVE_TABLES: Connection {:p} is streaming_active, getting separate pool connection",
            pg_conn
        ));
        let alt_conn = unsafe { pg_get_thread_connection((*pg_conn).db_path.as_ptr()) };
        if !alt_conn.is_null()
            && unsafe { !(*alt_conn).conn.is_null() }
            && alt_conn != pg_conn
            && unsafe { (*alt_conn).streaming_active.load(Ordering::SeqCst) == 0 }
        {
            resolve_conn = alt_conn;
            log_debug(&format!(
                "RESOLVE_TABLES: Using alternate connection {:p} for OID lookup",
                resolve_conn
            ));
        } else {
            log_debug("RESOLVE_TABLES: No alternate connection, skipping OID lookup to protect streaming");
            unsafe { (*pg_stmt).col_tables_resolved = 1 };
            return 0;
        }
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
    let res = unsafe { crate::libpq_helpers::rust_pq_exec((*resolve_conn).conn, query_cs.as_ptr()) } as *mut PgResultLibpq;

    if res.is_null() || crate::libpq_helpers::rust_pq_result_status(res) != PGRES_TUPLES_OK {
        log_error(&format!(
            "RESOLVE_TABLES: Query failed: {}",
            if res.is_null() {
                "NULL result".to_string()
            } else {
                cstr_to_string_or(unsafe { crate::libpq_helpers::rust_pq_error_message((*resolve_conn).conn) }, "?")
            }
        ));
        if !res.is_null() {
            crate::libpq_helpers::rust_pq_clear(res);
        }
        unsafe { (*pg_stmt).col_tables_resolved = 1 };
        return -1;
    }

    let num_results = crate::libpq_helpers::rust_pq_ntuples(res);
    let mut result_oids = vec![0u32; MAX_PARAMS];
    let mut result_names: Vec<[c_char; 64]> = vec![[0 as c_char; 64]; MAX_PARAMS];

    for i in 0..(num_results as usize).min(MAX_PARAMS) {
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

    for i in 0..(num_cols as usize).min(MAX_PARAMS) {
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
            for j in 0..(num_results as usize).min(MAX_PARAMS) {
                if result_oids[j] == table_oid {
                    let dup = unsafe { libc::strdup(result_names[j].as_ptr()) };
                    if !dup.is_null() {
                        unsafe { (*pg_stmt).col_table_names[i] = dup };
                        log_debug(&format!(
                            "RESOLVE_TABLES: col[{}] '{}' -> table '{}'",
                            i,
                            cstr_to_string_or(
                                unsafe {
                                    crate::db_interpose_helpers::rust_pg_result_col_name(
                                        helpers_result_ptr((*pg_stmt).result),
                                        i as c_int,
                                    )
                                },
                                "?"
                            ),
                            cstr_to_string_or(result_names[j].as_ptr(), "?")
                        ));
                    }
                    break;
                }
            }
        }
    }

    unsafe { (*pg_stmt).col_tables_resolved = 1 };
    log_info(&format!(
        "RESOLVE_TABLES: Resolved {} columns ({} from cache, {} from query)",
        num_cols, cache_hits, num_unique_tables
    ));
    0
}

fn pg_decode_bytea_cached_impl(pg_stmt: *mut PgStmt, row: c_int, col: c_int, out_length: *mut c_int) -> *const c_void {
    if pg_stmt.is_null() {
        if !out_length.is_null() {
            unsafe { *out_length = 0 };
        }
        return ptr::null();
    }

    unsafe {
        if (*pg_stmt).decoded_blob_row == row && !(*pg_stmt).decoded_blobs[col as usize].is_null() {
            if !out_length.is_null() {
                *out_length = (*pg_stmt).decoded_blob_lens[col as usize];
            }
            return (*pg_stmt).decoded_blobs[col as usize];
        }

        if (*pg_stmt).decoded_blob_row != row {
            crate::db_interpose_helpers::rust_step_clear_row_caches(
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                (*pg_stmt).decoded_blobs.as_mut_ptr(),
                (*pg_stmt).decoded_blob_lens.as_mut_ptr(),
                MAX_PARAMS as c_int,
                ptr::null_mut(),
                &mut (*pg_stmt).decoded_blob_row as *mut c_int,
            );
            (*pg_stmt).decoded_blob_row = row;
        }

        let mut decoded: *mut u8 = ptr::null_mut();
        let mut len: c_int = 0;
        let mut is_hex: c_int = 0;
        let mut is_null: c_int = 0;
        let ok = crate::db_interpose_helpers::rust_pg_decode_bytea(
            helpers_result_ptr((*pg_stmt).result),
            row,
            col,
            &mut decoded as *mut *mut u8,
            &mut len as *mut c_int,
            &mut is_hex as *mut c_int,
            &mut is_null as *mut c_int,
        );
        if ok == 0 || is_null != 0 || decoded.is_null() {
            if !out_length.is_null() {
                *out_length = 0;
            }
            return ptr::null();
        }

        if is_hex == 0 {
            if !out_length.is_null() {
                *out_length = len;
            }
            return decoded as *const c_void;
        }

        (*pg_stmt).decoded_blobs[col as usize] = decoded as *mut c_void;
        (*pg_stmt).decoded_blob_lens[col as usize] = len;
        if !out_length.is_null() {
            *out_length = len;
        }

        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                PMT_COLUMN_DECODED_BLOB_ALLOC,
                (len as u64).saturating_add(1),
                1,
            );
        }

        decoded as *const c_void
    }
}

#[no_mangle]
pub extern "C" fn rust_pg_decode_bytea_cached(
    pg_stmt: *mut PgStmt,
    row: c_int,
    col: c_int,
    out_length: *mut c_int,
) -> *const c_void {
    pg_decode_bytea_cached_impl(pg_stmt, row, col, out_length)
}

fn ensure_pg_result_for_metadata(pg_stmt: *mut PgStmt) -> bool {
    if pg_stmt.is_null() {
        return false;
    }

    unsafe {
        if !(*pg_stmt).result.is_null() || !(*pg_stmt).cached_result.is_null() {
            return true;
        }
        if (*pg_stmt).num_cols > 0 {
            return true;
        }
        if (*pg_stmt).pg_sql.is_null() || (*pg_stmt).conn.is_null() || (*(*pg_stmt).conn).conn.is_null() {
            return false;
        }
    }

    let conn = unsafe { (*pg_stmt).conn };
    let is_library = unsafe { crate::db_interpose_helpers::rust_is_library_db_path((*conn).db_path.as_ptr()) };
    if is_library == 0 {
        return false;
    }

    let mut exec_conn = conn;
    let thread_conn = unsafe { pg_get_thread_connection((*conn).db_path.as_ptr()) };
    if !thread_conn.is_null() && unsafe { (*thread_conn).is_pg_active != 0 } && unsafe { !(*thread_conn).conn.is_null() } {
        exec_conn = thread_conn;
    }

    if unsafe { (*exec_conn).streaming_active.load(Ordering::SeqCst) != 0 } {
        log_debug(&format!(
            "METADATA: skipping — connection {:p} is streaming_active",
            exec_conn
        ));
        return false;
    }

    let _conn_guard = unsafe { PthreadMutexGuard::lock(&mut (*exec_conn).mutex as *mut _) };

    if unsafe { (*exec_conn).streaming_active.load(Ordering::SeqCst) != 0 } {
        log_debug(&format!(
            "METADATA: skipping after lock — connection {:p} is streaming_active",
            exec_conn
        ));
        return false;
    }

    unsafe {
        crate::libpq_helpers::rust_pq_set_nonblocking((*exec_conn).conn, 0);
        while crate::libpq_helpers::rust_pq_is_busy((*exec_conn).conn) != 0 {
            crate::libpq_helpers::rust_pq_consume_input((*exec_conn).conn);
        }
        loop {
            let pending = crate::libpq_helpers::rust_pq_get_result((*exec_conn).conn);
            if pending.is_null() {
                break;
            }
            crate::libpq_helpers::rust_pq_clear(pending);
        }
    }

    let mut has_unbound_params = false;
    unsafe {
        if (*pg_stmt).param_count > 0 {
            has_unbound_params = true;
            for i in 0..(*pg_stmt).param_count as usize {
                if !(*pg_stmt).param_values[i].is_null() {
                    has_unbound_params = false;
                    break;
                }
            }
        }
    }

    unsafe {
        if has_unbound_params && (*pg_stmt).stmt_name[0] != 0 {
            log_info(&format!(
                "METADATA_DESCRIBE: Using prepared-statement describe for: {}",
                cstr_prefix((*pg_stmt).pg_sql, 100, "?")
            ));

            let mut cached_name: *const c_char = ptr::null();
            let cached = pg_stmt_cache_lookup(exec_conn, (*pg_stmt).sql_hash, &mut cached_name as *mut *const c_char);
            if cached == 0 {
                let prep = crate::libpq_helpers::rust_pq_prepare(
                    (*exec_conn).conn,
                    (*pg_stmt).stmt_name.as_ptr(),
                    (*pg_stmt).pg_sql,
                    0,
                    ptr::null(),
                ) as *mut PgResultLibpq;
                if crate::libpq_helpers::rust_pq_result_status(prep) != PGRES_COMMAND_OK {
                    if pg_is_duplicate_prepared_stmt(prep) == 0 {
                        log_error(&format!(
                            "METADATA_DESCRIBE: PQprepare failed: {}",
                            cstr_to_string_or(
                                crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn),
                                "?"
                            )
                        ));
                        crate::libpq_helpers::rust_pq_clear(prep);
                        return false;
                    }
                }
                pg_stmt_cache_add(exec_conn, (*pg_stmt).sql_hash, (*pg_stmt).stmt_name.as_ptr(), (*pg_stmt).param_count);
                crate::libpq_helpers::rust_pq_clear(prep);
            }

            let desc = crate::libpq_helpers::rust_pq_describe_prepared(
                (*exec_conn).conn,
                (*pg_stmt).stmt_name.as_ptr(),
            ) as *mut PgResultLibpq;

            if crate::libpq_helpers::rust_pq_result_status(desc) == PGRES_COMMAND_OK {
                (*pg_stmt).num_cols = crate::libpq_helpers::rust_pq_nfields(desc);
                if (*pg_stmt).num_cols > 0 {
                    let ncols = (*pg_stmt).num_cols as usize;
                    let col_names = libc::calloc(ncols, std::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
                    if !col_names.is_null() {
                        (*pg_stmt).col_names = col_names;
                        (*pg_stmt).num_col_names = (*pg_stmt).num_cols;
                        for i in 0..ncols {
                            let name = crate::db_interpose_helpers::rust_pg_result_col_name(
                                helpers_result_ptr(desc),
                                i as c_int,
                            );
                            if !name.is_null() {
                                let dup = libc::strdup(name);
                                *col_names.add(i) = dup;
                            }
                        }
                    }
                }
                (*pg_stmt).result = desc;
                (*pg_stmt).num_rows = 0;
                (*pg_stmt).current_row = 0;
                (*pg_stmt).metadata_only_result = 1;
                log_info(&format!(
                    "METADATA_DESCRIBE: Success - {} cols for: {}",
                    (*pg_stmt).num_cols,
                    cstr_prefix((*pg_stmt).pg_sql, 100, "?")
                ));
                return true;
            }

            log_error(&format!(
                "METADATA_DESCRIBE: PQdescribePrepared failed: {}",
                cstr_to_string_or(
                    crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn),
                    "?"
                )
            ));
            crate::libpq_helpers::rust_pq_clear(desc);
            return false;
        }
    }

    log_info(&format!(
        "METADATA_EXEC: Executing query for column metadata access: {}",
        cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 100, "?")
    ));

    let mut param_values: [*const c_char; MAX_PARAMS] = [ptr::null(); MAX_PARAMS];
    unsafe {
        for i in 0..(*pg_stmt).param_count as usize {
            param_values[i] = (*pg_stmt).param_values[i] as *const c_char;
        }
    }

    unsafe {
        (*pg_stmt).result = crate::libpq_helpers::rust_pq_exec_params(
            (*exec_conn).conn,
            (*pg_stmt).pg_sql,
            (*pg_stmt).param_count,
            ptr::null(),
            param_values.as_ptr(),
            ptr::null(),
            ptr::null(),
            0,
        ) as *mut PgResultLibpq;
    }

    let status = unsafe { crate::libpq_helpers::rust_pq_result_status((*pg_stmt).result) };
    if status == PGRES_TUPLES_OK {
        unsafe {
            (*pg_stmt).num_rows = crate::libpq_helpers::rust_pq_ntuples((*pg_stmt).result);
            (*pg_stmt).num_cols = crate::libpq_helpers::rust_pq_nfields((*pg_stmt).result);
            (*pg_stmt).current_row = -1;
            (*pg_stmt).result_conn = exec_conn;

            if rust_resolve_column_tables(pg_stmt, exec_conn) < 0 {
                log_error("Failed to resolve column tables");
            }

            (*pg_stmt).metadata_only_result = 1;
        }

        log_info(&format!(
            "METADATA_EXEC: Success - {} cols, {} rows (metadata_only=1)",
            unsafe { (*pg_stmt).num_cols },
            unsafe { (*pg_stmt).num_rows }
        ));
        true
    } else {
        log_error(&format!(
            "METADATA_EXEC: Query failed: {}",
            unsafe {
                cstr_to_string_or(
                    crate::libpq_helpers::rust_pq_error_message((*exec_conn).conn),
                    "?"
                )
            }
        ));
        unsafe {
            crate::libpq_helpers::rust_pq_clear((*pg_stmt).result);
            (*pg_stmt).result = ptr::null_mut();
        }
        false
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    log_debug(&format!("COLUMN_COUNT: stmt={:p}", p_stmt));
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };
        if !unsafe { (*pg_stmt).cached_result }.is_null() {
            let cached = unsafe { &*(*pg_stmt).cached_result };
            return cached.num_cols;
        }
        unsafe {
            if (*pg_stmt).num_cols == 0 && !(*pg_stmt).pg_sql.is_null() && (*pg_stmt).result.is_null() {
                ensure_pg_result_for_metadata(pg_stmt);
            }
            return (*pg_stmt).num_cols;
        }
    }
    unsafe { orig_sqlite3_column_count.map(|f| f(p_stmt)).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_type(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    unsafe {
        global_column_type_calls = global_column_type_calls.wrapping_add(1);
        let tls_calls = tls_column_type_calls_ptr();
        *tls_calls = (*tls_calls).wrapping_add(1);
    }
    log_debug(&format!("COLUMN_TYPE: stmt={:p} idx={}", p_stmt, idx));
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    let dbg_sql = if !pg_stmt.is_null() {
        unsafe {
            if !(*pg_stmt).pg_sql.is_null() {
                (*pg_stmt).pg_sql
            } else {
                (*pg_stmt).sql
            }
        }
    } else {
        ptr::null()
    };
    let dbg_db = unsafe { orig_sqlite3_db_handle.and_then(|f| Some(f(p_stmt))).unwrap_or(ptr::null_mut()) };
    unsafe {
        pg_exception_note_phase(
            b"column_type\0".as_ptr() as *const c_char,
            dbg_sql,
            p_stmt,
            dbg_db,
        );
    }

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        unsafe {
            last_query_being_processed = (*pg_stmt).pg_sql;
            let tls_query = tls_last_query_ptr();
            *tls_query = (*pg_stmt).pg_sql;
        }
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if !unsafe { (*pg_stmt).cached_result }.is_null() {
            let cached = unsafe { &*(*pg_stmt).cached_result };
            let row = unsafe { (*pg_stmt).current_row };
            if idx >= 0
                && (idx as i32) < cached.num_cols
                && row >= 0
                && row < cached.num_rows
            {
                let crow = unsafe { &*cached.rows.add(row as usize) };
                let is_null = unsafe { *crow.is_null.add(idx as usize) != 0 };
                let col_name = if !cached.col_names.is_null() {
                    unsafe { *cached.col_names.add(idx as usize) }
                } else {
                    ptr::null()
                };
                let trace_col = trace_badcast_should_log_col(pg_stmt, idx, col_name);
                if is_null {
                    log_debug(&format!(
                        "COLUMN_TYPE_VERBOSE: idx={} row={} -> SQLITE_NULL (cached, is_null=true)",
                        idx, row
                    ));
                    return SQLITE_NULL;
                }

                if !crow.values.is_null() {
                    let val_ptr = unsafe { *crow.values.add(idx as usize) };
                    if !val_ptr.is_null() {
                        let raw_val = pg_text_to_int64_impl(val_ptr);
                        let mut masked = 0i64;
                        if mask_collection_metadata_type(pg_stmt, col_name, raw_val, &mut masked) {
                            return SQLITE_NULL;
                        }
                    }
                }

                let oid = if !cached.col_types.is_null() {
                    unsafe { *cached.col_types.add(idx as usize) }
                } else {
                    0
                };
                let result = pg_oid_to_sqlite_type_impl(oid);
                if trace_col {
                    trace_badcast_log_ctx(pg_stmt, p_stmt, idx, "column_type", "cached", row, 0, oid, col_name);
                    log_debug(&format!(
                        "TRACE_BADCAST: column_type (cached) idx={} col='{}' row={} oid={} -> {} sql={}",
                        idx,
                        cstr_to_string_or(col_name, "?"),
                        row,
                        oid,
                        sqlite_type_name(result),
                        cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                    ));
                }
                log_debug(&format!(
                    "COLUMN_TYPE_VERBOSE: idx={} row={} OID={} -> {} (cached)",
                    idx,
                    row,
                    oid,
                    sqlite_type_name(result)
                ));
                return result;
            }

            log_debug(&format!(
                "COLUMN_TYPE_VERBOSE: idx={} row={} -> SQLITE_NULL (cached, out of bounds)",
                idx, row
            ));
            return SQLITE_NULL;
        }

        if unsafe { (*pg_stmt).result.is_null() } {
            log_debug(&format!(
                "COLUMN_TYPE_VERBOSE: idx={} -> SQLITE_NULL (no result)",
                idx
            ));
            return SQLITE_NULL;
        }

        if idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } {
            log_debug(&format!(
                "COL_TYPE_BOUNDS: idx={} out of bounds (num_cols={}) sql={}",
                idx,
                unsafe { (*pg_stmt).num_cols },
                cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 100, "?")
            ));
            return SQLITE_NULL;
        }

        let row = unsafe { (*pg_stmt).current_row };
        if row < 0 || row >= unsafe { (*pg_stmt).num_rows } {
            log_debug(&format!(
                "COL_TYPE_ROW_BOUNDS: row={} out of bounds (num_rows={})",
                row,
                unsafe { (*pg_stmt).num_rows }
            ));
            return SQLITE_NULL;
        }

        let mut is_null = 0;
        let mut oid_u: c_uint = 0;
        let mut sqlite_type = SQLITE_NULL;
        unsafe {
            crate::db_interpose_helpers::rust_pg_result_type_info(
                helpers_result_ptr((*pg_stmt).result),
                row,
                idx,
                &mut oid_u as *mut c_uint,
                &mut is_null as *mut c_int,
                &mut sqlite_type as *mut c_int,
            );
        }
        let oid = oid_u as u32;
        let col_name = unsafe {
            crate::db_interpose_helpers::rust_pg_result_col_name(helpers_result_ptr((*pg_stmt).result), idx)
        };
        let trace_col = trace_badcast_should_log_col(pg_stmt, idx, col_name);
        unsafe {
            last_column_being_accessed = col_name;
        }
        if is_null != 0 {
            log_debug(&format!(
                "COLUMN_TYPE: idx={} col='{}' is NULL, returning SQLITE_NULL",
                idx,
                cstr_to_string_or(col_name, "?")
            ));
            if trace_col {
                trace_badcast_log_ctx(pg_stmt, p_stmt, idx, "column_type", "live", row, 1, oid, col_name);
                log_debug(&format!(
                    "TRACE_BADCAST: column_type idx={} col='{}' row={} oid={} is_null=1 -> NULL sql={}",
                    idx,
                    cstr_to_string_or(col_name, "?"),
                    row,
                    oid,
                    cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                ));
            }
            return SQLITE_NULL;
        }

        let mut val_buf = [0 as c_char; 128];
        let val_len = unsafe {
            crate::db_interpose_helpers::rust_pg_result_text_copy(
                helpers_result_ptr((*pg_stmt).result),
                row,
                idx,
                val_buf.as_mut_ptr(),
                val_buf.len(),
            )
        };
        if val_len >= 0 {
            let raw_val = pg_text_to_int64_impl(val_buf.as_ptr());
            let mut masked = 0i64;
            if mask_collection_metadata_type(pg_stmt, col_name, raw_val, &mut masked) {
                return SQLITE_NULL;
            }
        }

        let result = sqlite_type;
        let col_decltype_guess = match oid {
            16 | 21 | 23 | 26 => "INTEGER",
            20 => "BIGINT",
            700 | 701 | 1700 => "REAL",
            17 => "BLOB",
            _ => "TEXT",
        };

        if trace_col {
            trace_badcast_log_ctx(pg_stmt, p_stmt, idx, "column_type", "live", row, 0, oid, col_name);
            log_debug(&format!(
                "TRACE_BADCAST: column_type idx={} col='{}' row={} oid={} is_null=0 -> {} (guess_decltype='{}') sql={}",
                idx,
                cstr_to_string_or(col_name, "?"),
                row,
                oid,
                sqlite_type_name(result),
                col_decltype_guess,
                cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
            ));
        }

        log_debug(&format!(
            "COLUMN_TYPE: idx={} col='{}' row={} OID={} is_null={} -> {} (decltype='{}')",
            idx,
            cstr_to_string_or(col_name, "?"),
            row,
            oid,
            is_null,
            sqlite_type_name(result),
            col_decltype_guess
        ));
        return result;
    }

    unsafe { orig_sqlite3_column_type.map(|f| f(p_stmt, idx)).unwrap_or(SQLITE_NULL) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_int(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    validate_type_consistency(p_stmt, idx, "column_int");
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    let trace = trace_badcast_should_log(pg_stmt, idx);

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if !unsafe { (*pg_stmt).cached_result }.is_null() {
            let cached = unsafe { &*(*pg_stmt).cached_result };
            let row = unsafe { (*pg_stmt).current_row };
            if idx >= 0
                && (idx as i32) < cached.num_cols
                && row >= 0
                && row < cached.num_rows
            {
                let crow = unsafe { &*cached.rows.add(row as usize) };
                let is_null = unsafe { *crow.is_null.add(idx as usize) != 0 };
                if !is_null {
                    let val_ptr = unsafe { *crow.values.add(idx as usize) };
                    if !val_ptr.is_null() {
                        let result_val = pg_text_to_int_impl(val_ptr);
                        let col_name = if !cached.col_names.is_null() {
                            unsafe { *cached.col_names.add(idx as usize) }
                        } else {
                            ptr::null()
                        };
                        if !col_name.is_null() {
                            let needle = NEEDLE_TYPE.as_ptr() as *const c_char;
                            if unsafe { !libc::strstr(col_name, needle).is_null() } {
                                log_debug(&format!(
                                    "TYPE_DEBUG_CACHED: col='{}' idx={} row={} raw_val='{}' result={} sql={}",
                                    cstr_to_string_or(col_name, "?"),
                                    idx,
                                    row,
                                    cstr_to_string_or(val_ptr, "?"),
                                    result_val,
                                    cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                                ));
                            }
                        }
                        return result_val;
                    }
                }
            }
            return 0;
        }

        if unsafe { (*pg_stmt).result.is_null() } {
            return 0;
        }
        if idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } {
            log_debug(&format!(
                "COL_INT_BOUNDS: idx={} out of bounds (num_cols={})",
                idx,
                unsafe { (*pg_stmt).num_cols }
            ));
            return 0;
        }
        let row = unsafe { (*pg_stmt).current_row };
        if row < 0 || row >= unsafe { (*pg_stmt).num_rows } {
            log_debug(&format!(
                "COL_INT_ROW_BOUNDS: row={} out of bounds (num_rows={})",
                row,
                unsafe { (*pg_stmt).num_rows }
            ));
            return 0;
        }

        let mut is_null = 0;
        let mut oid_u: c_uint = 0;
        let mut sqlite_type = SQLITE_NULL;
        unsafe {
            crate::db_interpose_helpers::rust_pg_result_type_info(
                helpers_result_ptr((*pg_stmt).result),
                row,
                idx,
                &mut oid_u as *mut c_uint,
                &mut is_null as *mut c_int,
                &mut sqlite_type as *mut c_int,
            );
        }
        let oid = oid_u as u32;
        let col_name = unsafe {
            crate::db_interpose_helpers::rust_pg_result_col_name(helpers_result_ptr((*pg_stmt).result), idx)
        };

        if trace && oid == 20 {
            trace_badcast_log_ctx(pg_stmt, p_stmt, idx, "column_int", "entry", row, 0, oid, col_name);
            log_debug(&format!(
                "TRACE_BADCAST_ACCESSOR: column_int called for oid=20 col='{}' idx={} sql={}",
                cstr_to_string_or(col_name, "?"),
                idx,
                cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
            ));
        }

        let mut result_val = 0;
        let mut val_ptr: *const c_char = ptr::null();
        if is_null == 0 {
            let mut val_buf = [0 as c_char; 128];
            let val_len = unsafe {
                crate::db_interpose_helpers::rust_pg_result_text_copy(
                    helpers_result_ptr((*pg_stmt).result),
                    row,
                    idx,
                    val_buf.as_mut_ptr(),
                    val_buf.len(),
                )
            };
            if val_len >= 0 {
                val_ptr = val_buf.as_ptr();
                result_val = pg_text_to_int_impl(val_ptr);
            }

            let mut masked = 0i64;
            if mask_collection_metadata_type(pg_stmt, col_name, result_val as i64, &mut masked) {
                result_val = masked as c_int;
            }
        }

        if !col_name.is_null() {
            let needle = NEEDLE_TYPE.as_ptr() as *const c_char;
            if unsafe { !libc::strstr(col_name, needle).is_null() } {
                log_debug(&format!(
                    "TYPE_DEBUG: col='{}' idx={} row={} raw_val='{}' result={} sql={}",
                    cstr_to_string_or(col_name, "?"),
                    idx,
                    row,
                    cstr_to_string_or(val_ptr, "(NULL)"),
                    result_val,
                    cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                ));
            }
        }

        return result_val;
    }

    unsafe { orig_sqlite3_column_int.map(|f| f(p_stmt, idx)).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_int64(p_stmt: *mut sqlite3_stmt, idx: c_int) -> i64 {
    validate_type_consistency(p_stmt, idx, "column_int64");
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    let trace = trace_badcast_should_log(pg_stmt, idx);

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if !unsafe { (*pg_stmt).cached_result }.is_null() {
            let cached = unsafe { &*(*pg_stmt).cached_result };
            let row = unsafe { (*pg_stmt).current_row };
            if idx >= 0
                && (idx as i32) < cached.num_cols
                && row >= 0
                && row < cached.num_rows
            {
                let crow = unsafe { &*cached.rows.add(row as usize) };
                let is_null = unsafe { *crow.is_null.add(idx as usize) != 0 };
                if !is_null {
                    let val_ptr = unsafe { *crow.values.add(idx as usize) };
                    if !val_ptr.is_null() {
                        let mut result_val = pg_text_to_int64_impl(val_ptr);
                        let col_name = if !cached.col_names.is_null() {
                            unsafe { *cached.col_names.add(idx as usize) }
                        } else {
                            ptr::null()
                        };
                        let mut masked = 0i64;
                        if mask_collection_metadata_type(pg_stmt, col_name, result_val, &mut masked) {
                            result_val = masked;
                        }
                        return result_val;
                    }
                }
            }
            return 0;
        }

        if unsafe { (*pg_stmt).result.is_null() } {
            return 0;
        }
        if idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } {
            log_debug(&format!(
                "COL_INT64_BOUNDS: idx={} out of bounds (num_cols={})",
                idx,
                unsafe { (*pg_stmt).num_cols }
            ));
            return 0;
        }
        let row = unsafe { (*pg_stmt).current_row };
        if row < 0 || row >= unsafe { (*pg_stmt).num_rows } {
            log_debug(&format!(
                "COL_INT64_ROW_BOUNDS: row={} out of bounds (num_rows={})",
                row,
                unsafe { (*pg_stmt).num_rows }
            ));
            return 0;
        }

        let mut is_null = 0;
        let mut oid_u: c_uint = 0;
        let mut sqlite_type = SQLITE_NULL;
        unsafe {
            crate::db_interpose_helpers::rust_pg_result_type_info(
                helpers_result_ptr((*pg_stmt).result),
                row,
                idx,
                &mut oid_u as *mut c_uint,
                &mut is_null as *mut c_int,
                &mut sqlite_type as *mut c_int,
            );
        }
        let oid = oid_u as u32;
        let col_name = unsafe {
            crate::db_interpose_helpers::rust_pg_result_col_name(helpers_result_ptr((*pg_stmt).result), idx)
        };

        if trace && oid == 20 {
            trace_badcast_log_ctx(pg_stmt, p_stmt, idx, "column_int64", "entry", row, 0, oid, col_name);
            log_debug(&format!(
                "TRACE_BADCAST_ACCESSOR: column_int64 called for oid=20 col='{}' idx={} sql={}",
                cstr_to_string_or(col_name, "?"),
                idx,
                cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
            ));
        }

        let mut result_val: i64 = 0;
        let mut val_ptr: *const c_char = ptr::null();
        if is_null == 0 {
            let mut val_buf = [0 as c_char; 128];
            let val_len = unsafe {
                crate::db_interpose_helpers::rust_pg_result_text_copy(
                    helpers_result_ptr((*pg_stmt).result),
                    row,
                    idx,
                    val_buf.as_mut_ptr(),
                    val_buf.len(),
                )
            };
            if val_len >= 0 {
                val_ptr = val_buf.as_ptr();
                result_val = pg_text_to_int64_impl(val_ptr);
            }
            let mut masked = 0i64;
            if mask_collection_metadata_type(pg_stmt, col_name, result_val, &mut masked) {
                result_val = masked;
            }

            if !col_name.is_null() {
                let needle = NEEDLE_TYPE.as_ptr() as *const c_char;
                if unsafe { !libc::strstr(col_name, needle).is_null() } {
                    log_debug(&format!(
                        "TYPE_DEBUG_INT64: col='{}' idx={} row={} raw_val='{}' result={} sql={}",
                        cstr_to_string_or(col_name, "?"),
                        idx,
                        row,
                        cstr_to_string_or(val_ptr, "(NULL)"),
                        result_val,
                        cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                    ));
                }
            }
        }

        return result_val;
    }

    unsafe { orig_sqlite3_column_int64.map(|f| f(p_stmt, idx)).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_double(p_stmt: *mut sqlite3_stmt, idx: c_int) -> f64 {
    validate_type_consistency(p_stmt, idx, "column_double");
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if !unsafe { (*pg_stmt).cached_result }.is_null() {
            let cached = unsafe { &*(*pg_stmt).cached_result };
            let row = unsafe { (*pg_stmt).current_row };
            if idx >= 0
                && (idx as i32) < cached.num_cols
                && row >= 0
                && row < cached.num_rows
            {
                let crow = unsafe { &*cached.rows.add(row as usize) };
                let is_null = unsafe { *crow.is_null.add(idx as usize) != 0 };
                if !is_null {
                    let val_ptr = unsafe { *crow.values.add(idx as usize) };
                    if !val_ptr.is_null() {
                        let result_val = pg_text_to_double_impl(val_ptr);
                        return result_val;
                    }
                }
            }
            return 0.0;
        }

        if unsafe { (*pg_stmt).result.is_null() } {
            return 0.0;
        }
        if idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } {
            return 0.0;
        }
        let row = unsafe { (*pg_stmt).current_row };
        if row < 0 || row >= unsafe { (*pg_stmt).num_rows } {
            return 0.0;
        }

        let mut is_null = 0;
        let mut oid_u: c_uint = 0;
        let mut sqlite_type = SQLITE_NULL;
        unsafe {
            crate::db_interpose_helpers::rust_pg_result_type_info(
                helpers_result_ptr((*pg_stmt).result),
                row,
                idx,
                &mut oid_u as *mut c_uint,
                &mut is_null as *mut c_int,
                &mut sqlite_type as *mut c_int,
            );
        }

        if is_null == 0 {
            let mut val_buf = [0 as c_char; 128];
            let val_len = unsafe {
                crate::db_interpose_helpers::rust_pg_result_text_copy(
                    helpers_result_ptr((*pg_stmt).result),
                    row,
                    idx,
                    val_buf.as_mut_ptr(),
                    val_buf.len(),
                )
            };
            if val_len >= 0 {
                return pg_text_to_double_impl(val_buf.as_ptr());
            }
        }
        return 0.0;
    }

    unsafe { orig_sqlite3_column_double.map(|f| f(p_stmt, idx)).unwrap_or(0.0) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_text(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_uchar {
    let dbg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    let dbg_sql = if !dbg_stmt.is_null() {
        unsafe {
            if !(*dbg_stmt).pg_sql.is_null() {
                (*dbg_stmt).pg_sql
            } else {
                (*dbg_stmt).sql
            }
        }
    } else {
        ptr::null()
    };
    let dbg_db = unsafe { orig_sqlite3_db_handle.and_then(|f| Some(f(p_stmt))).unwrap_or(ptr::null_mut()) };
    unsafe {
        pg_exception_note_phase(
            b"column_text\0".as_ptr() as *const c_char,
            dbg_sql,
            p_stmt,
            dbg_db,
        );
    }

    validate_type_consistency(p_stmt, idx, "column_text");

    let pg_stmt = dbg_stmt;
    if pg_stmt.is_null() {
        log_debug(&format!(
            "COLUMN_TEXT_NO_STMT: pStmt={:p} idx={} - statement not in registry (non-PG db, using SQLite fallback)",
            p_stmt, idx
        ));
    } else if unsafe { (*pg_stmt).is_pg == 0 } {
        log_debug(&format!(
            "COLUMN_TEXT_NOT_PG: pStmt={:p} idx={} is_pg=false, using SQLite fallback",
            p_stmt, idx
        ));
    }

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        log_debug(&format!(
            "COLUMN_TEXT: locked mutex, result={:p} row={} cols={}",
            unsafe { (*pg_stmt).result },
            unsafe { (*pg_stmt).current_row },
            unsafe { (*pg_stmt).num_cols }
        ));

        let mut source_value: *const c_char = ptr::null();
        let mut col_name: *const c_char = ptr::null();
        let mut oid: u32 = 0;

        if !unsafe { (*pg_stmt).cached_result }.is_null() {
            let cached = unsafe { &*(*pg_stmt).cached_result };
            let row = unsafe { (*pg_stmt).current_row };
            log_debug(&format!(
                "COLUMN_TEXT_CACHE: idx={} row={} num_cols={} num_rows={}",
                idx,
                row,
                cached.num_cols,
                cached.num_rows
            ));
            if idx >= 0
                && (idx as i32) < cached.num_cols
                && row >= 0
                && row < cached.num_rows
            {
                let crow = unsafe { &*cached.rows.add(row as usize) };
                let is_null = unsafe { *crow.is_null.add(idx as usize) != 0 };
                if !is_null {
                    let val_ptr = unsafe { *crow.values.add(idx as usize) };
                    if !val_ptr.is_null() {
                        source_value = val_ptr;
                        if !cached.col_names.is_null() {
                            col_name = unsafe { *cached.col_names.add(idx as usize) };
                        }
                        if !cached.col_types.is_null() {
                            oid = unsafe { *cached.col_types.add(idx as usize) };
                        }
                        log_debug(&format!(
                            "COLUMN_TEXT_CACHE_HIT: found cached value len={}",
                            unsafe { libc::strlen(source_value) }
                        ));
                    }
                }
            }
            if source_value.is_null() {
                log_debug(&format!(
                    "COLUMN_TEXT_CACHE_NULL: idx={} row={} returning NULL",
                    idx, row
                ));
                return ptr::null();
            }

            let str_len = unsafe { libc::strlen(source_value) } as usize;
            let buf_idx = next_text_buffer_index();
            let mut out_ptr: *const c_uchar = ptr::null();
            COLUMN_TEXT_BUFFERS.with(|bufs| {
                let mut bufs = bufs.borrow_mut();
                let buf = &mut bufs[buf_idx];
                let transform_rc = unsafe {
                    crate::db_interpose_helpers::rust_column_text_transform(
                        col_name,
                        oid as c_uint,
                        (*pg_stmt).pg_sql,
                        source_value,
                        str_len,
                        buf.as_mut_ptr() as *mut c_char,
                        TEXT_BUFFER_SIZE,
                    )
                };
                if transform_rc == -1 {
                    log_error(&format!(
                        "COLUMN_TEXT_UTF8_INVALID: idx={} row={} contains invalid UTF-8! len={} sql={}",
                        idx,
                        unsafe { (*pg_stmt).current_row },
                        str_len,
                        cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                    ));
                    out_ptr = buf.as_ptr();
                    return;
                }
                if transform_rc == 1 {
                    log_debug(&format!(
                        "COLUMN_TEXT_TRANSFORM: idx={} row={} '{:.80}' -> '{:.80}'",
                        idx,
                        unsafe { (*pg_stmt).current_row },
                        cstr_to_string_or(source_value, ""),
                        cstr_to_string_or(buf.as_ptr() as *const c_char, "")
                    ));
                    out_ptr = buf.as_ptr();
                    return;
                }

                let copy_len = if str_len < TEXT_BUFFER_SIZE - 1 {
                    str_len
                } else {
                    TEXT_BUFFER_SIZE - 1
                };
                if copy_len > 0 {
                    unsafe {
                        ptr::copy_nonoverlapping(source_value as *const u8, buf.as_mut_ptr(), copy_len);
                    }
                }
                buf[copy_len] = 0;
                log_debug(&format!(
                    "COLUMN_TEXT: copied {} bytes to buffer[{}] idx={} row={} utf8=valid",
                    copy_len,
                    buf_idx,
                    idx,
                    unsafe { (*pg_stmt).current_row }
                ));
                out_ptr = buf.as_ptr();
            });
            return out_ptr;
        }

        if unsafe { (*pg_stmt).result.is_null() } {
            log_debug("COLUMN_TEXT: no result, returning empty buffer");
            let buf_idx = next_text_buffer_index();
            let mut out_ptr: *const c_uchar = ptr::null();
            COLUMN_TEXT_BUFFERS.with(|bufs| {
                let mut bufs = bufs.borrow_mut();
                let buf = &mut bufs[buf_idx];
                buf[0] = 0;
                out_ptr = buf.as_ptr();
            });
            return out_ptr;
        }

        if idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } {
            log_debug(&format!(
                "COLUMN_TEXT: idx={} out of bounds (num_cols={})",
                idx,
                unsafe { (*pg_stmt).num_cols }
            ));
            let buf_idx = next_text_buffer_index();
            let mut out_ptr: *const c_uchar = ptr::null();
            COLUMN_TEXT_BUFFERS.with(|bufs| {
                let mut bufs = bufs.borrow_mut();
                let buf = &mut bufs[buf_idx];
                buf[0] = 0;
                out_ptr = buf.as_ptr();
            });
            return out_ptr;
        }

        let row = unsafe { (*pg_stmt).current_row };
        if row < 0 || row >= unsafe { (*pg_stmt).num_rows } {
            log_debug(&format!(
                "COLUMN_TEXT: row={} out of bounds (num_rows={})",
                row,
                unsafe { (*pg_stmt).num_rows }
            ));
            let buf_idx = next_text_buffer_index();
            let mut out_ptr: *const c_uchar = ptr::null();
            COLUMN_TEXT_BUFFERS.with(|bufs| {
                let mut bufs = bufs.borrow_mut();
                let buf = &mut bufs[buf_idx];
                buf[0] = 0;
                out_ptr = buf.as_ptr();
            });
            return out_ptr;
        }

        let mut is_null = 0;
        let mut oid_u: c_uint = 0;
        let mut sqlite_type = SQLITE_NULL;
        unsafe {
            crate::db_interpose_helpers::rust_pg_result_type_info(
                helpers_result_ptr((*pg_stmt).result),
                row,
                idx,
                &mut oid_u as *mut c_uint,
                &mut is_null as *mut c_int,
                &mut sqlite_type as *mut c_int,
            );
        }
        oid = oid_u as u32;
        col_name = unsafe {
            crate::db_interpose_helpers::rust_pg_result_col_name(helpers_result_ptr((*pg_stmt).result), idx)
        };
        if is_null != 0 {
            return ptr::null();
        }

        let buf_idx = next_text_buffer_index();
        let mut out_ptr: *const c_uchar = ptr::null();
        let mut preview = [0u8; 128];
        let mut source_len: usize = 0;
        let mut transform_rc: c_int = 0;
        COLUMN_TEXT_BUFFERS.with(|bufs| {
            let mut bufs = bufs.borrow_mut();
            let buf = &mut bufs[buf_idx];
            transform_rc = unsafe {
                crate::db_interpose_helpers::rust_pg_result_text_transform_copy(
                    helpers_result_ptr((*pg_stmt).result),
                    row,
                    idx,
                    col_name,
                    oid_u,
                    (*pg_stmt).pg_sql,
                    is_null,
                    buf.as_mut_ptr() as *mut c_char,
                    TEXT_BUFFER_SIZE,
                    preview.as_mut_ptr() as *mut c_char,
                    preview.len(),
                    &mut source_len as *mut usize,
                )
            };
            out_ptr = buf.as_ptr();
        });

        if transform_rc == -2 {
            return ptr::null();
        }
        if transform_rc == -1 {
            log_error(&format!(
                "COLUMN_TEXT_UTF8_INVALID: idx={} row={} contains invalid UTF-8! len={} sql={}",
                idx,
                unsafe { (*pg_stmt).current_row },
                source_len,
                cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
            ));
            return out_ptr;
        }
        if transform_rc < 0 {
            return out_ptr;
        }

        if matches!(oid, 23 | 20 | 21) {
            let preview_str = unsafe { CStr::from_ptr(preview.as_ptr() as *const c_char) }.to_string_lossy();
            log_debug(&format!(
                "COLUMN_TEXT_INTEGER: col='{}' idx={} row={} oid={} val='{:.50}' - INTEGER column accessed as TEXT!",
                cstr_to_string_or(col_name, "?"),
                idx,
                row,
                oid,
                preview_str
            ));
        }
        if !col_name.is_null() {
            let needle = NEEDLE_TYPE.as_ptr() as *const c_char;
            if unsafe { !libc::strstr(col_name, needle).is_null() } {
                let preview_str = unsafe { CStr::from_ptr(preview.as_ptr() as *const c_char) }.to_string_lossy();
                log_debug(&format!(
                    "TYPE_DEBUG_TEXT: col='{}' idx={} row={} val='{:.50}' sql={}",
                    cstr_to_string_or(col_name, "?"),
                    idx,
                    row,
                    preview_str,
                    cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                ));
            }
        }

        if transform_rc == 1 {
            let preview_str = unsafe { CStr::from_ptr(preview.as_ptr() as *const c_char) }.to_string_lossy();
            log_debug(&format!(
                "COLUMN_TEXT_TRANSFORM: idx={} row={} '{:.80}' -> '{:.80}'",
                idx,
                unsafe { (*pg_stmt).current_row },
                preview_str,
                cstr_to_string_or(out_ptr as *const c_char, "")
            ));
            return out_ptr;
        }

        log_debug(&format!(
            "COLUMN_TEXT: copied {} bytes to buffer[{}] idx={} row={} utf8=valid",
            source_len,
            buf_idx,
            idx,
            unsafe { (*pg_stmt).current_row }
        ));

        return out_ptr;
    }

    log_debug("COLUMN_TEXT: falling through to orig");
    unsafe {
        orig_sqlite3_column_text
            .map(|f| f(p_stmt, idx))
            .unwrap_or(ptr::null())
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_blob(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_void {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };

    if pg_stmt.is_null() || unsafe { (*pg_stmt).is_pg == 0 } {
        return unsafe { orig_sqlite3_column_blob.map(|f| f(p_stmt, idx)).unwrap_or(ptr::null()) };
    }

    let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

    if !unsafe { (*pg_stmt).cached_result }.is_null() {
        let cached = unsafe { &*(*pg_stmt).cached_result };
        let row = unsafe { (*pg_stmt).current_row };
        if idx >= 0
            && (idx as i32) < cached.num_cols
            && row >= 0
            && row < cached.num_rows
        {
            let crow = unsafe { &*cached.rows.add(row as usize) };
            let is_null = unsafe { *crow.is_null.add(idx as usize) != 0 };
            if !is_null {
                let val_ptr = unsafe { *crow.values.add(idx as usize) };
                if !val_ptr.is_null() {
                    return val_ptr as *const c_void;
                }
            }
        }
        return ptr::null();
    }

    if unsafe { (*pg_stmt).result.is_null() } {
        return ptr::null();
    }
    if idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } || (idx as usize) >= MAX_PARAMS {
        return ptr::null();
    }

    let row = unsafe { (*pg_stmt).current_row };
    if row < 0 || row >= unsafe { (*pg_stmt).num_rows } {
        return ptr::null();
    }

    let mut is_null = 0;
    let mut oid_u: c_uint = 0;
    let mut sqlite_type = SQLITE_NULL;
    unsafe {
        crate::db_interpose_helpers::rust_pg_result_type_info(
            helpers_result_ptr((*pg_stmt).result),
            row,
            idx,
            &mut oid_u as *mut c_uint,
            &mut is_null as *mut c_int,
            &mut sqlite_type as *mut c_int,
        );
    }
    if is_null != 0 {
        return ptr::null();
    }

    let col_name = unsafe {
        crate::db_interpose_helpers::rust_pg_result_col_name(helpers_result_ptr((*pg_stmt).result), idx)
    };
    log_debug(&format!(
        "column_blob called: col={} name={} type={} row={}",
        idx,
        cstr_to_string_or(col_name, "?"),
        oid_u,
        row
    ));

    if oid_u == 17 {
        let mut blob_len = 0;
        return pg_decode_bytea_cached_impl(pg_stmt, row, idx, &mut blob_len as *mut c_int);
    }

    if unsafe { (*pg_stmt).cached_row } == row && !unsafe { (*pg_stmt).cached_blob[idx as usize] }.is_null() {
        return unsafe { (*pg_stmt).cached_blob[idx as usize] } as *const c_void;
    }

    if unsafe { (*pg_stmt).cached_row } != row {
        unsafe {
            crate::db_interpose_helpers::rust_step_clear_row_caches(
                (*pg_stmt).cached_text.as_mut_ptr(),
                (*pg_stmt).cached_blob.as_mut_ptr(),
                (*pg_stmt).cached_blob_len.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                MAX_PARAMS as c_int,
                &mut (*pg_stmt).cached_row as *mut c_int,
                ptr::null_mut(),
            );
            (*pg_stmt).cached_row = row;
        }
    }

    let blob_len = unsafe {
        crate::db_interpose_helpers::rust_pg_result_length(helpers_result_ptr((*pg_stmt).result), row, idx)
    };
    if blob_len > 0 {
        let buf = unsafe { libc::malloc(blob_len as usize) } as *mut u8;
        if buf.is_null() {
            log_error(&format!(
                "COL_BLOB: malloc failed for column {}, len {}",
                idx, blob_len
            ));
            return ptr::null();
        }
        let copied = unsafe {
            crate::db_interpose_helpers::rust_pg_result_blob_copy(
                helpers_result_ptr((*pg_stmt).result),
                row,
                idx,
                buf,
                blob_len as usize,
            )
        };
        if copied <= 0 {
            unsafe { libc::free(buf as *mut c_void) };
            unsafe {
                (*pg_stmt).cached_blob[idx as usize] = ptr::null_mut();
                (*pg_stmt).cached_blob_len[idx as usize] = 0;
            }
            return ptr::null();
        }
        unsafe {
            (*pg_stmt).cached_blob[idx as usize] = buf as *mut c_void;
            (*pg_stmt).cached_blob_len[idx as usize] = copied;
        }
        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                PMT_COLUMN_CACHED_BLOB_ALLOC,
                copied as u64,
                1,
            );
        }
    }

    (unsafe { (*pg_stmt).cached_blob[idx as usize] }) as *const c_void
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_bytes(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    log_debug(&format!("COLUMN_BYTES: stmt={:p} idx={}", p_stmt, idx));
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };

    if pg_stmt.is_null() || unsafe { (*pg_stmt).is_pg == 0 } {
        return unsafe { orig_sqlite3_column_bytes.map(|f| f(p_stmt, idx)).unwrap_or(0) };
    }

    let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

    if !unsafe { (*pg_stmt).cached_result }.is_null() {
        let cached = unsafe { &*(*pg_stmt).cached_result };
        let row = unsafe { (*pg_stmt).current_row };
        if idx >= 0
            && (idx as i32) < cached.num_cols
            && row >= 0
            && row < cached.num_rows
        {
            let crow = unsafe { &*cached.rows.add(row as usize) };
            let is_null = unsafe { *crow.is_null.add(idx as usize) != 0 };
            if !is_null {
                if !crow.lengths.is_null() {
                    return unsafe { *crow.lengths.add(idx as usize) };
                }
            }
        }
        return 0;
    }

    if unsafe { (*pg_stmt).result.is_null() } {
        return 0;
    }
    if idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } {
        return 0;
    }

    let row = unsafe { (*pg_stmt).current_row };
    if row < 0 || row >= unsafe { (*pg_stmt).num_rows } {
        return 0;
    }

    let mut is_null = 0;
    let mut oid_u: c_uint = 0;
    let mut sqlite_type = SQLITE_NULL;
    unsafe {
        crate::db_interpose_helpers::rust_pg_result_type_info(
            helpers_result_ptr((*pg_stmt).result),
            row,
            idx,
            &mut oid_u as *mut c_uint,
            &mut is_null as *mut c_int,
            &mut sqlite_type as *mut c_int,
        );
    }
    if is_null != 0 {
        return 0;
    }

    if oid_u == 17 {
        let mut blob_len = 0;
        pg_decode_bytea_cached_impl(pg_stmt, row, idx, &mut blob_len as *mut c_int);
        return blob_len;
    }

    let len = unsafe {
        crate::db_interpose_helpers::rust_pg_result_length(helpers_result_ptr((*pg_stmt).result), row, idx)
    };
    if len < 0 {
        0
    } else {
        len
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_name(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    log_debug(&format!("COLUMN_NAME: stmt={:p} idx={}", p_stmt, idx));
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    let mut result: *const c_char = ptr::null();
    let mut use_orig = true;

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if unsafe { (*pg_stmt).result.is_null() }
            && unsafe { (*pg_stmt).cached_result.is_null() }
            && unsafe { (*pg_stmt).col_names.is_null() }
            && !unsafe { (*pg_stmt).pg_sql.is_null() }
        {
            if !ensure_pg_result_for_metadata(pg_stmt) {
                log_debug("COLUMN_NAME: failed to execute query for metadata");
            }
        }

        if !unsafe { (*pg_stmt).col_names.is_null() }
            && idx >= 0
            && idx < unsafe { (*pg_stmt).num_col_names }
        {
            result = unsafe { *(*pg_stmt).col_names.add(idx as usize) };
            log_debug(&format!(
                "COLUMN_NAME: returning '{}' for idx={} (from col_names)",
                cstr_to_string_or(result, "NULL"),
                idx
            ));
            use_orig = false;
        } else if !unsafe { (*pg_stmt).result.is_null() }
            && idx >= 0
            && idx < unsafe { (*pg_stmt).num_cols }
        {
            result = unsafe {
                crate::db_interpose_helpers::rust_pg_result_col_name(
                    helpers_result_ptr((*pg_stmt).result),
                    idx,
                )
            };
            log_debug(&format!(
                "COLUMN_NAME: returning '{}' for idx={}",
                cstr_to_string_or(result, "NULL"),
                idx
            ));
            use_orig = false;
        } else if unsafe { (*pg_stmt).result.is_null() } && unsafe { (*pg_stmt).col_names.is_null() } {
            log_debug("COLUMN_NAME: pg_stmt has no result or col_names, falling back to orig");
            use_orig = true;
        } else {
            log_debug(&format!(
                "COLUMN_NAME: idx out of bounds (num_cols={}, num_col_names={})",
                unsafe { (*pg_stmt).num_cols },
                unsafe { (*pg_stmt).num_col_names }
            ));
            use_orig = false;
        }
    } else {
        log_debug(&format!(
            "COLUMN_NAME: not a PG stmt (pg_stmt={:p} is_pg={}), using orig",
            pg_stmt,
            if pg_stmt.is_null() { -1 } else { unsafe { (*pg_stmt).is_pg } }
        ));
    }

    if use_orig {
        result = unsafe { orig_sqlite3_column_name.map(|f| f(p_stmt, idx)).unwrap_or(ptr::null()) };
        log_debug(&format!(
            "COLUMN_NAME: orig returned '{}'",
            cstr_to_string_or(result, "NULL")
        ));
    }
    result
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_decltype(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    log_debug(&format!("DECLTYPE_ENTRY: stmt={:p} idx={}", p_stmt, idx));
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    let trace_any = trace_badcast_should_log(pg_stmt, idx);

    log_debug(&format!(
        "DECLTYPE_CALLED: stmt={:p} idx={} pg_stmt={:p} is_pg={}",
        p_stmt,
        idx,
        pg_stmt,
        if pg_stmt.is_null() { -1 } else { unsafe { (*pg_stmt).is_pg } }
    ));

    if pg_stmt.is_null() || unsafe { (*pg_stmt).is_pg == 0 } {
        log_debug(&format!(
            "DECLTYPE_PASSTHROUGH: non-PG stmt={:p}, using orig_sqlite3_column_decltype",
            p_stmt
        ));
        return unsafe { orig_sqlite3_column_decltype.map(|f| f(p_stmt, idx)).unwrap_or(ptr::null()) };
    }

    let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

    if unsafe { (*pg_stmt).result.is_null() }
        && unsafe { (*pg_stmt).cached_result.is_null() }
        && !unsafe { (*pg_stmt).pg_sql.is_null() }
    {
        if !ensure_pg_result_for_metadata(pg_stmt) {
            log_error("COLUMN_DECLTYPE: failed to execute query for metadata, returning TEXT");
            if trace_any {
                log_info(&format!(
                    "TRACE_BADCAST: column_decltype idx={} -> TEXT (metadata exec failed) sql={}",
                    idx,
                    cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                ));
            }
            return DECLTYPE_TEXT.as_ptr() as *const c_char;
        }
    }

    if unsafe { (*pg_stmt).result.is_null() } || idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } {
        log_debug(&format!(
            "DECLTYPE_NO_RESULT: result={:p} idx={} num_cols={}, returning TEXT",
            unsafe { (*pg_stmt).result },
            idx,
            unsafe { (*pg_stmt).num_cols }
        ));
        if trace_any {
            trace_badcast_log_ctx(pg_stmt, p_stmt, idx, "column_decltype", "noresult", unsafe { (*pg_stmt).current_row }, 0, 0, ptr::null());
            log_debug(&format!(
                "TRACE_BADCAST: column_decltype idx={} -> TEXT (no result / oob) sql={}",
                idx,
                cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
            ));
        }
        return DECLTYPE_TEXT.as_ptr() as *const c_char;
    }

    let col_name = unsafe {
        crate::db_interpose_helpers::rust_pg_result_col_name(helpers_result_ptr((*pg_stmt).result), idx)
    };
    let trace = trace_badcast_should_log_col(pg_stmt, idx, col_name);
    let mut cached_type = lookup_sqlite_decltype(unsafe { (*pg_stmt).conn }, col_name);

    let is_metadata_type = if !col_name.is_null() {
        let needle = NEEDLE_METADATA_TYPE.as_ptr() as *const c_char;
        unsafe { !libc::strstr(col_name, needle).is_null() }
    } else {
        false
    };
    if is_metadata_type {
        log_debug(&format!(
            "DECLTYPE_DEBUG: START col='{}' idx={} row={} num_cols={}",
            cstr_to_string_or(col_name, "?"),
            idx,
            unsafe { (*pg_stmt).current_row },
            unsafe { (*pg_stmt).num_cols }
        ));
    }

    if is_metadata_type {
        log_debug(&format!(
            "DECLTYPE_DEBUG: STEP1 col='{}' cached_type='{}'",
            cstr_to_string_or(col_name, "?"),
            cstr_to_string_or(cached_type, "(null)")
        ));
    }

    if cached_type.is_null() && idx >= 0 && (idx as usize) < MAX_PARAMS {
        let table_ptr = unsafe { (*pg_stmt).col_table_names[idx as usize] };
        if !table_ptr.is_null() {
            let table = unsafe { CStr::from_ptr(table_ptr) }.to_string_lossy();
            let column = cstr_to_string_or(col_name, "");
            let mut cache_key = String::with_capacity(DECLTYPE_MAX_KEY_LEN);
            cache_key.push_str(&table);
            cache_key.push('_');
            cache_key.push_str(&column);
            cached_type = lookup_decltype_direct(unsafe { (*pg_stmt).conn }, &cache_key);
            if !cached_type.is_null() {
                log_info(&format!(
                    "DECLTYPE_RESOLVED: bare col '{}' -> table '{}' -> '{}'",
                    column,
                    table,
                    cstr_to_string_or(cached_type, "?")
                ));
            }
            if is_metadata_type {
                log_debug(&format!(
                    "DECLTYPE_DEBUG: STEP2 table='{}' cache_key='{}' cached_type='{}'",
                    table,
                    cache_key,
                    cstr_to_string_or(cached_type, "(null)")
                ));
            }
        } else if is_metadata_type {
            log_debug(&format!(
                "DECLTYPE_DEBUG: STEP2 SKIPPED (cached_type={} idx={} has_table=0)",
                if cached_type.is_null() { "(null)" } else { "set" },
                idx
            ));
        }
    } else if is_metadata_type {
        log_debug(&format!(
            "DECLTYPE_DEBUG: STEP2 SKIPPED (cached_type={} idx={} has_table={})",
            if cached_type.is_null() { "(null)" } else { "set" },
            idx,
            if idx >= 0 && (idx as usize) < MAX_PARAMS && !unsafe { (*pg_stmt).col_table_names[idx as usize] }.is_null() { 1 } else { 0 }
        ));
    }

    if !cached_type.is_null() {
        if is_metadata_type {
            log_debug(&format!(
                "DECLTYPE_DEBUG: RETURNING CACHED='{}' for col='{}' idx={}",
                cstr_to_string_or(cached_type, "?"),
                cstr_to_string_or(col_name, "?"),
                idx
            ));
        }
        log_debug(&format!(
            "DECLTYPE_CACHED: idx={} col='{}' -> '{}' sql={}",
            idx,
            cstr_to_string_or(col_name, "?"),
            cstr_to_string_or(cached_type, "?"),
            cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 300, "?")
        ));
        if trace {
            let oid = unsafe {
                crate::db_interpose_helpers::rust_pg_result_col_oid(helpers_result_ptr((*pg_stmt).result), idx)
            };
            trace_badcast_log_ctx(pg_stmt, p_stmt, idx, "column_decltype", "cached", unsafe { (*pg_stmt).current_row }, 0, oid, col_name);
            log_debug(&format!(
                "TRACE_BADCAST: column_decltype (cached) idx={} col='{}' oid={} -> '{}' sql={}",
                idx,
                cstr_to_string_or(col_name, "?"),
                oid,
                cstr_to_string_or(cached_type, "?"),
                cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
            ));
        }
        return cached_type;
    }

    let oid = unsafe {
        crate::db_interpose_helpers::rust_pg_result_col_oid(helpers_result_ptr((*pg_stmt).result), idx)
    };
    let table_oid = unsafe {
        crate::db_interpose_helpers::rust_pg_result_col_table_oid(helpers_result_ptr((*pg_stmt).result), idx)
    };
    let special_case = unsafe {
        crate::pg_statement::rust_decltype_special_case(
            oid,
            col_name,
            (*pg_stmt).pg_sql,
            table_oid,
        )
    };

    if special_case == PG_DECLTYPE_CASE_DT_INTEGER_8 {
        if trace {
            trace_badcast_log_ctx(
                pg_stmt,
                p_stmt,
                idx,
                "column_decltype",
                "dt_integer(8)",
                unsafe { (*pg_stmt).current_row },
                0,
                oid,
                col_name,
            );
        }
        return DECLTYPE_DT_INTEGER_8.as_ptr() as *const c_char;
    }
    if special_case == PG_DECLTYPE_CASE_NULL {
        log_debug(&format!(
            "DECLTYPE_EXPR: col='{}' idx={} -> returning NULL (PQftable=InvalidOid, expression column)",
            cstr_to_string_or(col_name, "?"),
            idx
        ));
        return ptr::null();
    }

    let decltype = crate::pg_statement::oid_to_sqlite_decltype(oid).as_ptr();

    if trace {
        trace_badcast_log_ctx(
            pg_stmt,
            p_stmt,
            idx,
            "column_decltype",
            "oid",
            unsafe { (*pg_stmt).current_row },
            0,
            oid,
            col_name,
        );
        log_debug(&format!(
            "TRACE_BADCAST: column_decltype idx={} col='{}' oid={} -> '{}' sql={}",
            idx,
            cstr_to_string_or(col_name, "?"),
            oid,
            cstr_to_string_or(decltype, "(null)"),
            cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
        ));
    }

    if unsafe { libc::strcmp(decltype, DECLTYPE_BIGINT.as_ptr() as *const c_char) == 0 }
        || unsafe { libc::strcmp(decltype, DECLTYPE_INTEGER.as_ptr() as *const c_char) == 0 }
    {
        log_debug(&format!(
            "DECLTYPE_INT: col='{}' idx={} oid={} -> '{}' sql={}",
            cstr_to_string_or(col_name, "?"),
            idx,
            oid,
            cstr_to_string_or(decltype, "?"),
            cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 100, "?")
        ));
    }

    log_debug(&format!(
        "DECLTYPE_CACHED: idx={} col='{}' -> '{}' sql={}",
        idx,
        cstr_to_string_or(col_name, "?"),
        cstr_to_string_or(decltype, "?"),
        cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 100, "?")
    ));
    decltype
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_column_value(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *mut sqlite3_value {
    let dbg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    let dbg_sql = if !dbg_stmt.is_null() {
        unsafe {
            if !(*dbg_stmt).pg_sql.is_null() {
                (*dbg_stmt).pg_sql
            } else {
                (*dbg_stmt).sql
            }
        }
    } else {
        ptr::null()
    };
    let dbg_db = unsafe { orig_sqlite3_db_handle.and_then(|f| Some(f(p_stmt))).unwrap_or(ptr::null_mut()) };
    unsafe {
        pg_exception_note_phase(
            b"column_value\0".as_ptr() as *const c_char,
            dbg_sql,
            p_stmt,
            dbg_db,
        );
    }

    let pg_stmt = dbg_stmt;
    if pg_stmt.is_null() || unsafe { (*pg_stmt).is_pg == 0 } {
        return unsafe { orig_sqlite3_column_value.map(|f| f(p_stmt, idx)).unwrap_or(ptr::null_mut()) };
    }

    if let Ok(val) = std::env::var("PLEX_PG_DISABLE_COLUMN_VALUE") {
        if !val.is_empty() {
            let first = val.as_bytes()[0];
            if matches!(first, b'1' | b'y' | b'Y' | b't' | b'T') {
                log_error("COLUMN_VALUE: disabled via PLEX_PG_DISABLE_COLUMN_VALUE");
                return ptr::null_mut();
            }
        }
    }

    let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

    if unsafe { (*pg_stmt).result.is_null() }
        && unsafe { (*pg_stmt).cached_result.is_null() }
        && !unsafe { (*pg_stmt).pg_sql.is_null() }
    {
        if !ensure_pg_result_for_metadata(pg_stmt) {
            log_debug("COLUMN_VALUE: failed to execute query for metadata");
            return unsafe { orig_sqlite3_column_value.map(|f| f(p_stmt, idx)).unwrap_or(ptr::null_mut()) };
        }
    }

    if unsafe { (*pg_stmt).result.is_null() } {
        return unsafe { orig_sqlite3_column_value.map(|f| f(p_stmt, idx)).unwrap_or(ptr::null_mut()) };
    }

    if idx < 0 || idx >= unsafe { (*pg_stmt).num_cols } {
        log_debug(&format!(
            "COLUMN_VALUE_BOUNDS: idx={} out of bounds (num_cols={}) sql={}",
            idx,
            unsafe { (*pg_stmt).num_cols },
            cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 100, "?")
        ));
        return ptr::null_mut();
    }

    let row = unsafe { (*pg_stmt).current_row };

    let _fake_guard = unsafe {
        PthreadMutexGuard::lock(std::ptr::addr_of_mut!(fake_value_mutex) as *mut _)
    };
    let slot = unsafe {
        let cur = fake_value_next;
        fake_value_next = fake_value_next.wrapping_add(1);
        (cur as usize) & (MAX_FAKE_VALUES - 1)
    };
    let fake = unsafe { &mut fake_value_pool[slot] };
    fake.magic = PG_FAKE_VALUE_MAGIC;
    fake.pg_stmt = pg_stmt as *mut c_void;
    fake.col_idx = idx;
    fake.row_idx = row;
    fake.owner_thread = unsafe { libc::pthread_self() };
    fake as *mut PgFakeValue as *mut sqlite3_value
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_data_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    log_debug(&format!("DATA_COUNT: stmt={:p}", p_stmt));
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };

    if !pg_stmt.is_null() && unsafe { (*pg_stmt).is_pg != 0 } {
        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };
        let count = if unsafe { (*pg_stmt).current_row } < unsafe { (*pg_stmt).num_rows } {
            unsafe { (*pg_stmt).num_cols }
        } else {
            0
        };
        log_debug(&format!(
            "DATA_COUNT: returning {} (row={} rows={} cols={})",
            count,
            unsafe { (*pg_stmt).current_row },
            unsafe { (*pg_stmt).num_rows },
            unsafe { (*pg_stmt).num_cols }
        ));
        return count;
    }

    unsafe { orig_sqlite3_data_count.map(|f| f(p_stmt)).unwrap_or(0) }
}

#[cfg(test)]
mod tests {
    use super::{next_text_buffer_index, COLUMN_TEXT_BUF_IDX, NUM_TEXT_BUFFERS};
    use std::collections::HashSet;

    fn reset_text_buffer_idx() {
        COLUMN_TEXT_BUF_IDX.with(|idx| idx.set(0));
    }

    #[test]
    fn column_text_buffer_wraps_after_num_buffers() {
        reset_text_buffer_idx();

        let mut indices = Vec::with_capacity(NUM_TEXT_BUFFERS);
        for _ in 0..NUM_TEXT_BUFFERS {
            indices.push(next_text_buffer_index());
        }

        let unique: HashSet<usize> = indices.iter().copied().collect();
        assert_eq!(unique.len(), NUM_TEXT_BUFFERS);

        let wrapped = next_text_buffer_index();
        assert_eq!(wrapped, 0);
    }

    #[test]
    fn column_text_buffer_thread_local_indices_start_at_zero() {
        reset_text_buffer_idx();
        assert_eq!(next_text_buffer_index(), 0);

        let child_first = std::thread::spawn(|| {
            COLUMN_TEXT_BUF_IDX.with(|idx| idx.set(0));
            next_text_buffer_index()
        })
        .join()
        .expect("thread should join");

        assert_eq!(child_first, 0);
    }
}
