use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use crate::db_interpose_conn_utils::{cstr_prefix, cstr_to_string_or, log_debug, log_error, log_info};
use crate::db_interpose_common::{tls_in_interpose_call_ptr, tls_prepare_v2_depth_ptr};
use crate::db_interpose_prepare_utils::{
    contains_ascii_icase, contains_icase_ptr, starts_with_ascii_icase,
};
use crate::ffi_types::{sqlite3, sqlite3_stmt, PgConnection, PgStmt};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_ROW: c_int = 100;
const SQLITE_NOMEM: c_int = 7;

const WORKER_DELEGATION_THRESHOLD: isize = 400_000;

static TXN_ROUTE_TOTAL: AtomicU64 = AtomicU64::new(0);
static TXN_ROUTE_SKIPPED: AtomicU64 = AtomicU64::new(0);
static TXN_ROUTE_PG: AtomicU64 = AtomicU64::new(0);

static DISABLE_PREPARED_CACHED: AtomicI32 = AtomicI32::new(-1);

thread_local! {
    static STACK_LOG_COUNTER: Cell<i32> = Cell::new(0);
    static QUERY_LOOP_LOG_COUNTER: Cell<i32> = Cell::new(0);
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
    static mut worker_running: c_int;

    static mut shim_sqlite3_prepare_v2: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_char,
            c_int,
            *mut *mut sqlite3_stmt,
            *mut *const c_char,
        ) -> c_int,
    >;

    static mut orig_sqlite3_step: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_column_text: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const u8>;
    static mut orig_sqlite3_finalize: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_errmsg: Option<unsafe extern "C" fn(*mut sqlite3) -> *const c_char>;
    static mut orig_sqlite3_errcode: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int>;
    static mut orig_sqlite3_prepare16_v2: Option<
        unsafe extern "C" fn(
            *mut sqlite3,
            *const c_void,
            c_int,
            *mut *mut sqlite3_stmt,
            *mut *const c_void,
        ) -> c_int,
    >;

    fn sqlite3_prepare16_v2(
        db: *mut sqlite3,
        sql: *const c_void,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_void,
    ) -> c_int;

    fn ensure_real_sqlite_loaded();
    fn delegate_prepare_to_worker(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;

    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
    fn pg_exception_note_query(sql: *const c_char);

    fn pg_note_stmt_prepare(stmt: *mut sqlite3_stmt, sql: *const c_char);
    fn pg_stmt_create(conn: *mut PgConnection, sql: *const c_char, stmt: *mut sqlite3_stmt) -> *mut PgStmt;
    fn pg_register_stmt(sqlite_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt);
    fn rewrite_blobs_schema_migrations(sql: *const c_char, db_path: *const c_char) -> *mut c_char;
    fn pg_hash_sql(sql: *const c_char) -> u64;

    fn sql_translate(sql: *const c_char) -> SqlTranslation;
    fn sql_translation_free(result: *mut SqlTranslation);
}

struct PrepareDepthGuard {
    active: bool,
}

impl PrepareDepthGuard {
    unsafe fn enter() -> Self {
        let depth = tls_prepare_v2_depth_ptr();
        *depth += 1;
        Self { active: true }
    }

    unsafe fn decrement_now(&mut self) {
        if self.active {
            let depth = tls_prepare_v2_depth_ptr();
            *depth -= 1;
            self.active = false;
        }
    }
}

impl Drop for PrepareDepthGuard {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                let depth = tls_prepare_v2_depth_ptr();
                *depth -= 1;
            }
        }
    }
}

fn trace_prepare_sql_ok(sql: *const c_char) -> bool {
    crate::db_interpose_helpers::rust_trace_prepare_sql_ok(sql) != 0
}

fn trace_prepare_pgsql_if_enabled(sqlite_sql: *const c_char, pg_sql: *const c_char) {
    if !trace_prepare_sql_ok(sqlite_sql) {
        return;
    }
    if pg_sql.is_null() {
        return;
    }
    log_debug(&format!("TRACE_PREPARE_PGSQL: {}", cstr_prefix(pg_sql, 900, "")));
}

fn prepared_statements_disabled() -> bool {
    let cached = DISABLE_PREPARED_CACHED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let name = b"PLEX_PG_DISABLE_PREPARED\0";
    let val = unsafe {
        let env = libc::getenv(name.as_ptr() as *const c_char);
        crate::db_interpose_helpers::rust_env_truthy(env)
    };
    let flag = if val != 0 { 1 } else { 0 };
    DISABLE_PREPARED_CACHED.store(flag, Ordering::Relaxed);
    flag == 1
}

fn is_txn_control_sql(sql: *const c_char) -> bool {
    if sql.is_null() {
        return false;
    }
    let bytes = unsafe { CStr::from_ptr(sql).to_bytes() };
    let mut i = 0usize;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    let rest = &bytes[i..];
    starts_with_ascii_icase(rest, b"begin")
        || starts_with_ascii_icase(rest, b"commit")
        || starts_with_ascii_icase(rest, b"rollback")
        || starts_with_ascii_icase(rest, b"savepoint")
        || starts_with_ascii_icase(rest, b"release savepoint")
}

fn detect_query_loop(sql: *const c_char) -> bool {
    if sql.is_null() {
        return false;
    }
    let s = match unsafe { CStr::from_ptr(sql).to_str() } {
        Ok(s) => s,
        Err(_) => return false,
    };
    if let Some((count, elapsed_ms)) = crate::db_interpose_prepare_helpers::prepare_query_loop_tick(s) {
        QUERY_LOOP_LOG_COUNTER.with(|c| {
            let cur = c.get();
            if cur % 10 == 0 {
                log_info(&format!(
                    "High-frequency query: {} calls in {} ms (likely batch operation with different params) sql={}",
                    count,
                    elapsed_ms,
                    cstr_prefix(sql, 200, "NULL")
                ));
            }
            c.set(cur.wrapping_add(1));
        });
    }
    false
}

unsafe fn column_exists_in_sqlite(db: *mut sqlite3, table_name: *const c_char, column_name: *const c_char) -> bool {
    if db.is_null() || table_name.is_null() || column_name.is_null() {
        return false;
    }
    let prepare = match shim_sqlite3_prepare_v2 {
        Some(f) => f,
        None => return false,
    };

    let mut pragma_sql = [0 as c_char; 512];
    libc::snprintf(
        pragma_sql.as_mut_ptr(),
        pragma_sql.len(),
        b"PRAGMA table_info(%s)\0".as_ptr() as *const c_char,
        table_name,
    );

    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    let rc = prepare(db, pragma_sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
    if rc != SQLITE_OK || stmt.is_null() {
        return false;
    }

    let mut found = false;
    if let Some(step) = orig_sqlite3_step {
        while step(stmt) == SQLITE_ROW {
            let col_ptr = match orig_sqlite3_column_text {
                Some(col) => col(stmt, 1) as *const c_char,
                None => ptr::null(),
            };
            if !col_ptr.is_null() {
                let col = CStr::from_ptr(col_ptr).to_bytes();
                let want = CStr::from_ptr(column_name).to_bytes();
                if col.eq_ignore_ascii_case(want) {
                    found = true;
                    break;
                }
            }
        }
    }

    if let Some(fin) = orig_sqlite3_finalize {
        fin(stmt);
    }
    found
}

unsafe fn maybe_skip_alter_table_add(
    db: *mut sqlite3,
    z_sql: *const c_char,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> Option<c_int> {
    if z_sql.is_null() {
        return None;
    }
    if !contains_icase_ptr(z_sql, "ALTER TABLE") || !contains_icase_ptr(z_sql, " ADD ") {
        return None;
    }

    let table_pos = libc::strcasestr(z_sql, b"ALTER TABLE\0".as_ptr() as *const c_char);
    if table_pos.is_null() {
        return None;
    }
    let mut table_start = table_pos.add(11);
    while *table_start == b' ' as c_char {
        table_start = table_start.add(1);
    }

    let mut table_name = [0 as c_char; 256];
    if *table_start == b'\'' as c_char || *table_start == b'"' as c_char {
        let quote = *table_start;
        table_start = table_start.add(1);
        let end = libc::strchr(table_start, quote as i32);
        if !end.is_null() {
            let len = (end as usize).saturating_sub(table_start as usize);
            if len < table_name.len() {
                ptr::copy_nonoverlapping(table_start as *const u8, table_name.as_mut_ptr() as *mut u8, len);
            }
        }
    } else {
        let mut i = 0usize;
        while *table_start.add(i) != 0 && *table_start.add(i) != b' ' as c_char && i < table_name.len() - 1 {
            table_name[i] = *table_start.add(i);
            i += 1;
        }
    }

    if table_name[0] == 0 {
        return None;
    }

    let add_pos = libc::strcasestr(z_sql, b" ADD \0".as_ptr() as *const c_char);
    if add_pos.is_null() {
        return None;
    }
    let mut add_ptr = add_pos.add(5);
    while *add_ptr == b' ' as c_char {
        add_ptr = add_ptr.add(1);
    }

    let mut column_name = [0 as c_char; 256];
    if *add_ptr == b'\'' as c_char || *add_ptr == b'"' as c_char {
        let quote = *add_ptr;
        add_ptr = add_ptr.add(1);
        let end = libc::strchr(add_ptr, quote as i32);
        if !end.is_null() {
            let len = (end as usize).saturating_sub(add_ptr as usize);
            if len < column_name.len() {
                ptr::copy_nonoverlapping(add_ptr as *const u8, column_name.as_mut_ptr() as *mut u8, len);
            }
        }
    } else {
        let mut i = 0usize;
        while *add_ptr.add(i) != 0 && *add_ptr.add(i) != b' ' as c_char && i < column_name.len() - 1 {
            column_name[i] = *add_ptr.add(i);
            i += 1;
        }
    }

    if column_name[0] == 0 {
        return None;
    }

    if column_exists_in_sqlite(db, table_name.as_ptr(), column_name.as_ptr()) {
        log_info(&format!(
            "ALTER TABLE ADD COLUMN skipped (column '{}' already exists in '{}')",
            cstr_to_string_or(column_name.as_ptr(), ""),
            cstr_to_string_or(table_name.as_ptr(), "")
        ));
        if let Some(prepare) = shim_sqlite3_prepare_v2 {
            let rc = prepare(db, b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char, -1, pp_stmt, pz_tail);
            if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
                pg_note_stmt_prepare(*pp_stmt, b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char);
            }
            return Some(rc);
        }
        if !pp_stmt.is_null() {
            *pp_stmt = ptr::null_mut();
        }
        if !pz_tail.is_null() {
            *pz_tail = ptr::null();
        }
        return Some(SQLITE_OK);
    }

    None
}

unsafe fn copy_param_names(pg_stmt: *mut PgStmt, trans: &SqlTranslation) {
    if pg_stmt.is_null() {
        return;
    }
    if trans.param_names.is_null() || trans.param_count <= 0 {
        return;
    }
    let count = trans.param_count as usize;
    let alloc = libc::malloc(count * std::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
    if alloc.is_null() {
        return;
    }
    for i in 0..count {
        let name_ptr = *trans.param_names.add(i);
        *alloc.add(i) = if name_ptr.is_null() { ptr::null_mut() } else { libc::strdup(name_ptr) };
    }
    (*pg_stmt).param_names = alloc;
}

unsafe fn apply_prepared_stmt_settings(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }
    if (*pg_stmt).pg_sql.is_null() {
        return;
    }
    (*pg_stmt).sql_hash = pg_hash_sql((*pg_stmt).pg_sql);
    if !prepared_statements_disabled() {
        libc::snprintf(
            (*pg_stmt).stmt_name.as_mut_ptr(),
            (*pg_stmt).stmt_name.len(),
            b"ps_%llx\0".as_ptr() as *const c_char,
            (*pg_stmt).sql_hash as libc::c_ulonglong,
        );
        (*pg_stmt).use_prepared = 1;
    } else {
        (*pg_stmt).use_prepared = 0;
        (*pg_stmt).stmt_name[0] = 0;
    }
}

unsafe fn log_stack_info(stack_size: isize, stack_used: isize, stack_remaining: isize) {
    STACK_LOG_COUNTER.with(|c| {
        let cur = c.get().wrapping_add(1);
        c.set(cur);
        if cur == 1 || cur % 1000 == 0 {
            log_info(&format!(
                "STACK_CHECK: size={}KB used={}KB remaining={}KB (threshold=64KB)",
                stack_size / 1024,
                stack_used / 1024,
                stack_remaining / 1024
            ));
        }
    });
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare_v2_internal(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
    from_worker: c_int,
) -> c_int {
    unsafe {
        pg_exception_note_phase(
            b"prepare_v2\0".as_ptr() as *const c_char,
            z_sql,
            ptr::null_mut::<sqlite3_stmt>(),
            db,
        );
        if !z_sql.is_null() {
            pg_exception_note_query(z_sql);
        }
    }

    if trace_prepare_sql_ok(z_sql) {
        log_debug(&format!("TRACE_PREPARE_SQL: {}", cstr_prefix(z_sql, 700, "NULL")));
    }

    unsafe {
        if let Some(rc) = maybe_skip_alter_table_add(db, z_sql, pp_stmt, pz_tail) {
            return rc;
        }
    }

    if detect_query_loop(z_sql) {
        unsafe {
            if let Some(prepare) = shim_sqlite3_prepare_v2 {
                let rc = prepare(db, b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char, -1, pp_stmt, pz_tail);
                if rc == SQLITE_OK && !pp_stmt.is_null() && !(*pp_stmt).is_null() {
                    pg_note_stmt_prepare(*pp_stmt, b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char);
                }
                return rc;
            }
            if !pp_stmt.is_null() {
                *pp_stmt = ptr::null_mut();
            }
            if !pz_tail.is_null() {
                *pz_tail = ptr::null();
            }
            return SQLITE_OK;
        }
    }

    let mut depth_guard = unsafe { PrepareDepthGuard::enter() };
    unsafe {
        let depth = *tls_prepare_v2_depth_ptr();
        if depth > 50 {
            log_error(&format!(
                "RECURSION LIMIT: prepare_v2 called {} times (depth={})!",
                depth, depth
            ));
            log_error("  This indicates infinite recursion - ABORTING to prevent crash");
            log_error(&format!("  Query: {}", cstr_prefix(z_sql, 200, "NULL")));
            if !pp_stmt.is_null() {
                *pp_stmt = ptr::null_mut();
            }
            if !pz_tail.is_null() {
                *pz_tail = ptr::null();
            }
            return SQLITE_ERROR;
        }
    }

    let self_thread = unsafe { libc::pthread_self() };

    #[cfg(target_os = "macos")]
    let (stack_addr, stack_size, _stack_bottom) = unsafe {
        (
            libc::pthread_get_stackaddr_np(self_thread) as *mut c_void,
            libc::pthread_get_stacksize_np(self_thread),
            ptr::null_mut::<c_void>(),
        )
    };

    #[cfg(not(target_os = "macos"))]
    let (stack_addr, stack_size, stack_bottom) = unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        let mut stack_size_out: usize = 0;
        let mut stack_bottom_out: *mut c_void = ptr::null_mut();
        if libc::pthread_getattr_np(self_thread, &mut attr) == 0 {
            libc::pthread_attr_getstack(&attr, &mut stack_bottom_out, &mut stack_size_out);
            libc::pthread_attr_destroy(&mut attr);
        }
        let stack_addr_out = (stack_bottom_out as *mut u8).add(stack_size_out) as *mut c_void;
        (stack_addr_out, stack_size_out, stack_bottom_out)
    };

    let stack_base = stack_addr as isize;
    let local_var: u8 = 0;
    let current_stack = (&local_var as *const u8) as isize;
    let stack_used = stack_base.wrapping_sub(current_stack).abs();
    #[cfg(not(target_os = "macos"))]
    let mut stack_used = stack_used;
    #[cfg(not(target_os = "macos"))]
    let mut stack_size = stack_size;

    #[cfg(not(target_os = "macos"))]
    unsafe {
        if !stack_bottom.is_null() && !stack_addr.is_null() {
            let cur = current_stack as usize;
            let bottom = stack_bottom as usize;
            let top = stack_addr as usize;
            if cur < bottom || cur > top {
                log_error(&format!(
                    "STACK CALCULATION ERROR: current={:p} not in [{:p}, {:p}]",
                    current_stack as *const c_void, stack_bottom, stack_addr
                ));
                stack_size = 8 * 1024 * 1024;
                stack_used = 0;
            }
        }
    }

    let stack_remaining = stack_size as isize - stack_used;
    unsafe { log_stack_info(stack_size as isize, stack_used, stack_remaining) };

    let worker_active = unsafe { std::ptr::read_volatile(std::ptr::addr_of!(worker_running)) != 0 };
    if from_worker == 0 && stack_remaining < WORKER_DELEGATION_THRESHOLD && worker_active {
        log_debug(&format!(
            "WORKER DELEGATION: stack_remaining={} bytes < {}, delegating to 8MB worker",
            stack_remaining, WORKER_DELEGATION_THRESHOLD
        ));
        unsafe { depth_guard.decrement_now() };
        let rc = unsafe { delegate_prepare_to_worker(db, z_sql, n_byte, pp_stmt, pz_tail) };
        return rc;
    }

    let is_ondeck_query = z_sql != ptr::null()
        && ((contains_icase_ptr(z_sql, "metadata_item_settings") && contains_icase_ptr(z_sql, "metadata_items"))
            || (contains_icase_ptr(z_sql, "metadata_item_views") && contains_icase_ptr(z_sql, "grandparents"))
            || contains_icase_ptr(z_sql, "grandparentsSettings"));

    if is_ondeck_query && stack_remaining < 100_000 {
        log_info(&format!(
            "STACK LOW OnDeck: {} bytes remaining - using PG fast path",
            stack_remaining
        ));

        let pg_conn = crate::pg_client::rust_pg_find_connection(db);
        if !pg_conn.is_null()
            && unsafe { (*pg_conn).is_pg_active } != 0
            && unsafe { !(*pg_conn).conn.is_null() }
            && crate::db_interpose_helpers::rust_is_library_db_path(unsafe { (*pg_conn).db_path.as_ptr() }) != 0
        {
            let rc = if let Some(prepare) = unsafe { shim_sqlite3_prepare_v2 } {
                unsafe { prepare(db, b"SELECT 1\0".as_ptr() as *const c_char, -1, pp_stmt, pz_tail) }
            } else {
                if !pp_stmt.is_null() {
                    unsafe { *pp_stmt = ptr::null_mut() };
                }
                SQLITE_ERROR
            };

            if rc == SQLITE_OK && !pp_stmt.is_null() && unsafe { !(*pp_stmt).is_null() } {
                unsafe {
                    pg_note_stmt_prepare(*pp_stmt, b"SELECT 1\0".as_ptr() as *const c_char);
                }
                let pg_stmt = unsafe { pg_stmt_create(pg_conn, z_sql, *pp_stmt) };
                if !pg_stmt.is_null() {
                    unsafe {
                        (*pg_stmt).is_pg = 2;
                    }
                    let mut trans = unsafe { sql_translate(z_sql) };
                    if trans.success != 0 && !trans.sql.is_null() {
                        let aliased =
                            crate::db_interpose_prepare_helpers::alias_collection_sync_aggregates(
                                &cstr_to_string_or(z_sql, ""),
                                &cstr_to_string_or(trans.sql, ""),
                            );
                        let pg_sql_src = match aliased {
                            Some(ref s) => CString::new(s.as_str()).ok(),
                            None => None,
                        };
                        let pg_sql_ptr = if let Some(cs) = pg_sql_src.as_ref() {
                            unsafe { libc::strdup(cs.as_ptr()) }
                        } else {
                            unsafe { libc::strdup(trans.sql) }
                        };
                        unsafe {
                            (*pg_stmt).pg_sql = pg_sql_ptr;
                            (*pg_stmt).param_count = trans.param_count;
                        }
                        trace_prepare_pgsql_if_enabled(z_sql, unsafe { (*pg_stmt).pg_sql });
                        log_info(&format!(
                            "STACK LOW OnDeck: routed to PG: {}",
                            cstr_prefix(trans.sql, 100, "NULL")
                        ));
                    }
                    unsafe { sql_translation_free(&mut trans as *mut SqlTranslation) };
                }
            }
            return rc;
        }

        log_error("STACK CRITICAL OnDeck: no PG connection, returning empty");
        let rc = if let Some(prepare) = unsafe { shim_sqlite3_prepare_v2 } {
            unsafe { prepare(
                db,
                b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char,
                -1,
                pp_stmt,
                pz_tail,
            ) }
        } else {
            if !pp_stmt.is_null() {
                unsafe { *pp_stmt = ptr::null_mut() };
            }
            SQLITE_ERROR
        };
        if rc == SQLITE_OK && !pp_stmt.is_null() && unsafe { !(*pp_stmt).is_null() } {
            unsafe {
                pg_note_stmt_prepare(*pp_stmt, b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char);
            }
        }
        return rc;
    }

    let stack_threshold = if from_worker != 0 { 32_000 } else { 64_000 };
    if stack_remaining < stack_threshold {
        let pg_conn_check = crate::pg_client::rust_pg_find_connection(db);
        let is_pg_read = !pg_conn_check.is_null()
            && unsafe { (*pg_conn_check).is_pg_active } != 0
            && unsafe { !(*pg_conn_check).conn.is_null() }
            && !z_sql.is_null()
            && crate::pg_config::pg_config_is_read_operation(z_sql) != 0
            && crate::db_interpose_helpers::rust_is_library_db_path(unsafe { (*pg_conn_check).db_path.as_ptr() }) != 0;

        if is_pg_read {
            log_info(&format!(
                "STACK LOW ({} bytes) but using PG path for: {}",
                stack_remaining,
                cstr_prefix(z_sql, 100, "NULL")
            ));

            let rc = if let Some(prepare) = unsafe { shim_sqlite3_prepare_v2 } {
                unsafe { prepare(db, b"SELECT 1\0".as_ptr() as *const c_char, -1, pp_stmt, pz_tail) }
            } else {
                SQLITE_ERROR
            };

            if rc == SQLITE_OK && !pp_stmt.is_null() && unsafe { !(*pp_stmt).is_null() } {
                unsafe {
                    pg_note_stmt_prepare(*pp_stmt, b"SELECT 1\0".as_ptr() as *const c_char);
                }
                let pg_stmt = unsafe { pg_stmt_create(pg_conn_check, z_sql, *pp_stmt) };
                if !pg_stmt.is_null() {
                    unsafe {
                        (*pg_stmt).is_pg = 2;
                    }

                    let mut trans = unsafe { sql_translate(z_sql) };
                    if trans.success != 0 && !trans.sql.is_null() {
                        let aliased =
                            crate::db_interpose_prepare_helpers::alias_collection_sync_aggregates(
                                &cstr_to_string_or(z_sql, ""),
                                &cstr_to_string_or(trans.sql, ""),
                            );
                        let pg_sql_ptr = if let Some(s) = aliased {
                            let cs = CString::new(s).ok();
                            if let Some(cs) = cs.as_ref() {
                                unsafe { libc::strdup(cs.as_ptr()) }
                            } else {
                                unsafe { libc::strdup(trans.sql) }
                            }
                        } else {
                            unsafe { libc::strdup(trans.sql) }
                        };

                        unsafe {
                            (*pg_stmt).pg_sql = pg_sql_ptr;
                            (*pg_stmt).param_count = trans.param_count;
                        }
                        trace_prepare_pgsql_if_enabled(z_sql, unsafe { (*pg_stmt).pg_sql });

                        unsafe {
                            copy_param_names(pg_stmt, &trans);
                            apply_prepared_stmt_settings(pg_stmt);
                        }
                    }
                    unsafe { sql_translation_free(&mut trans as *mut SqlTranslation) };
                    unsafe { pg_register_stmt(*pp_stmt, pg_stmt) };
                }
            }
            return rc;
        }

        log_error(&format!(
            "STACK PROTECTION TRIGGERED: stack_used={}/{} bytes, remaining={} bytes",
            stack_used, stack_size, stack_remaining
        ));
        log_error(&format!(
            "  Query rejected (not a PG read): {}",
            cstr_prefix(z_sql, 200, "NULL")
        ));

        let pg_conn = crate::pg_client::rust_pg_find_connection(db);
        if !pg_conn.is_null() {
            unsafe {
                (*pg_conn).last_error_code = SQLITE_NOMEM;
                libc::snprintf(
                    (*pg_conn).last_error.as_mut_ptr(),
                    (*pg_conn).last_error.len(),
                    b"Stack protection: insufficient stack space (remaining=%ld).\0"
                        .as_ptr() as *const c_char,
                    stack_remaining as libc::c_long,
                );
            }
        }

        if !pp_stmt.is_null() {
            unsafe { *pp_stmt = ptr::null_mut() };
        }
        if !pz_tail.is_null() {
            unsafe { *pz_tail = ptr::null() };
        }
        return SQLITE_NOMEM;
    }

    let mut skip_complex_processing = 0;
    if from_worker == 0 && stack_remaining < 64_000 {
        skip_complex_processing = 1;
        log_info(&format!(
            "STACK CAUTION: stack_used={}/{} bytes, remaining={} - skipping complex processing",
            stack_used, stack_size, stack_remaining
        ));
    }

    if z_sql.is_null() {
        log_error("prepare_v2 called with NULL SQL");
        let rc = if let Some(prepare) = unsafe { shim_sqlite3_prepare_v2 } {
            unsafe { prepare(db, z_sql, n_byte, pp_stmt, pz_tail) }
        } else {
            if !pp_stmt.is_null() {
                unsafe { *pp_stmt = ptr::null_mut() };
            }
            SQLITE_ERROR
        };
        return rc;
    }

    let bytes = unsafe { CStr::from_ptr(z_sql).to_bytes() };
    if bytes.iter().any(|b| *b == b'`') {
        log_debug(&format!(
            "BACKTICK_QUERY: skip_complex={} len={} sql={}",
            skip_complex_processing,
            bytes.len(),
            cstr_prefix(z_sql, 200, "NULL")
        ));
    }

    if skip_complex_processing == 0
        && starts_with_ascii_icase(bytes, b"INSERT")
        && contains_ascii_icase(bytes, b"metadata_items")
    {
        log_info(&format!(
            "PREPARE_V2 INSERT metadata_items: {}",
            cstr_prefix(z_sql, 300, "NULL")
        ));
        if contains_ascii_icase(bytes, b"icu_root") {
            log_info("PREPARE_V2 has icu_root - will clean!");
        }
    }

    let pg_conn = if skip_complex_processing != 0 {
        ptr::null_mut()
    } else {
        crate::pg_client::rust_pg_find_connection(db)
    };

    let is_write = crate::pg_config::pg_config_is_write_operation(z_sql) != 0;
    let is_read = crate::pg_config::pg_config_is_read_operation(z_sql) != 0;

    if is_txn_control_sql(z_sql) {
        let total = TXN_ROUTE_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
        let skip_now = crate::pg_config::pg_config_should_skip_sql(z_sql) != 0;
        if skip_now {
            TXN_ROUTE_SKIPPED.fetch_add(1, Ordering::Relaxed);
        }
        if !pg_conn.is_null()
            && unsafe { (*pg_conn).is_pg_active } != 0
            && crate::db_interpose_helpers::rust_is_library_db_path(unsafe { (*pg_conn).db_path.as_ptr() }) != 0
            && (is_read || is_write)
            && !skip_now
        {
            TXN_ROUTE_PG.fetch_add(1, Ordering::Relaxed);
        }

        log_info(&format!(
            "TXN_ROUTE prepare: skip={} is_write={} is_read={} sql={}",
            skip_now as i32,
            is_write as i32,
            is_read as i32,
            cstr_prefix(z_sql, 220, "NULL")
        ));

        if total == 1 || total % 50 == 0 {
            let skipped = TXN_ROUTE_SKIPPED.load(Ordering::Relaxed);
            let routed_pg = TXN_ROUTE_PG.load(Ordering::Relaxed);
            log_info(&format!(
                "TXN_ROUTE stats: total={} skipped={} pg_routed={}",
                total, skipped, routed_pg
            ));
        }
    }

    if contains_icase_ptr(z_sql, "plugins") {
        log_info(&format!(
            "SKIP_DEBUG plugins query skip={} sql={}",
            crate::pg_config::pg_config_should_skip_sql(z_sql) != 0,
            cstr_prefix(z_sql, 220, "NULL")
        ));
    }

    let mut cleaned_sql: Option<CString> = None;
    let mut sql_for_sqlite = z_sql;
    let mut use_dummy_shadow = false;

    if !pg_conn.is_null()
        && unsafe { (*pg_conn).is_pg_active } != 0
        && crate::db_interpose_helpers::rust_is_library_db_path(unsafe { (*pg_conn).db_path.as_ptr() }) != 0
        && (is_read || is_write)
        && crate::pg_config::pg_config_should_skip_sql(z_sql) == 0
    {
        use_dummy_shadow = true;
    }

    let mut pre_trans: SqlTranslation = unsafe { std::mem::zeroed() };
    let mut have_pre_trans = false;

    let rc: c_int;

    if use_dummy_shadow {
        pre_trans = unsafe { sql_translate(z_sql) };
        have_pre_trans = true;

        if contains_icase_ptr(z_sql, "json_each(") {
            log_info(&format!("JSON_EACH_TRANSLATE: orig={}", cstr_prefix(z_sql, 220, "NULL")));
            log_info(&format!(
                "JSON_EACH_TRANSLATE: rc={} err={} out={}",
                pre_trans.success,
                if pre_trans.error[0] != 0 {
                    cstr_to_string_or(pre_trans.error.as_ptr(), "(null)")
                } else {
                    "(null)".to_string()
                },
                cstr_prefix(pre_trans.sql, 220, "(null)")
            ));
        }

        if contains_icase_ptr(z_sql, "metadata_item_settings") && contains_icase_ptr(z_sql, "metadata_items") {
            let q_count = bytes.iter().filter(|b| **b == b'?').count();
            let mut out_q_count = 0usize;
            if !pre_trans.sql.is_null() {
                let out_bytes = unsafe { CStr::from_ptr(pre_trans.sql).to_bytes() };
                out_q_count = out_bytes.iter().filter(|b| **b == b'?').count();
            }
            log_info(&format!("MIS_TRANSLATE: orig={}", cstr_prefix(z_sql, 1000, "NULL")));
            log_info(&format!(
                "MIS_TRANSLATE: rc={} params={} q_orig={} q_out={} out={}",
                pre_trans.success,
                pre_trans.param_count,
                q_count,
                out_q_count,
                cstr_prefix(pre_trans.sql, 1000, "(null)")
            ));
        }

        if !pre_trans.sql.is_null() {
            let orig_q = bytes.iter().filter(|b| **b == b'?').count() as i32;
            if orig_q > pre_trans.param_count {
                let hay = bytes;
                let mut pos = None;
                for (i, b) in hay.iter().enumerate() {
                    if *b == b'?' {
                        pos = Some(i);
                        break;
                    }
                }
                if let Some(pos) = pos {
                    let start = pos.saturating_sub(60);
                    let snippet = String::from_utf8_lossy(&hay[start..hay.len().min(start + 160)]).into_owned();
                    log_error(&format!(
                        "PLACEHOLDER_MISMATCH: orig_q={} translated_params={} around='{}'",
                        orig_q, pre_trans.param_count, snippet
                    ));
                } else {
                    log_error(&format!(
                        "PLACEHOLDER_MISMATCH: orig_q={} translated_params={} (no snippet)",
                        orig_q, pre_trans.param_count
                    ));
                }
            }
        }

        let param_count = pre_trans.param_count;

        let dummy_sql = if param_count == 0 {
            "SELECT 1 WHERE 0".to_string()
        } else {
            let has_names = !pre_trans.param_names.is_null();
            let mut out = String::from("SELECT 1 WHERE ");
            for i in 0..param_count {
                if i > 0 {
                    out.push_str(" AND ");
                }
                if has_names {
                    unsafe {
                        let name_ptr = *pre_trans.param_names.add(i as usize);
                        if !name_ptr.is_null() {
                            let name = CStr::from_ptr(name_ptr).to_string_lossy();
                            out.push(':');
                            out.push_str(&name);
                            out.push_str(" IS NOT NULL");
                        } else {
                            out.push_str("? IS NOT NULL");
                        }
                    }
                } else {
                    out.push_str("? IS NOT NULL");
                }
                if out.len() >= 4096 - 40 {
                    break;
                }
            }
            out
        };

        let dummy_c = CString::new(dummy_sql).ok();
        let dummy_ptr = dummy_c
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or_else(|| b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char);

        if let Some(prepare) = unsafe { shim_sqlite3_prepare_v2 } {
            rc = unsafe { prepare(db, dummy_ptr, -1, pp_stmt, pz_tail) };
        } else {
            log_error("CRITICAL: shim_sqlite3_prepare_v2 not initialized!");
            rc = SQLITE_ERROR;
            if !pp_stmt.is_null() {
                unsafe { *pp_stmt = ptr::null_mut() };
            }
        }

        if rc == SQLITE_OK && !pp_stmt.is_null() && unsafe { !(*pp_stmt).is_null() } {
            unsafe {
                pg_note_stmt_prepare(*pp_stmt, dummy_ptr);
            }
        } else {
            log_error(&format!(
                "PREPARE: Dummy shadow prepare failed (rc={}, params={}): {} dummy={}",
                rc,
                param_count,
                cstr_prefix(z_sql, 100, "NULL"),
                cstr_prefix(dummy_ptr, 200, "NULL")
            ));
            unsafe { sql_translation_free(&mut pre_trans as *mut SqlTranslation) };
            return rc;
        }

        log_debug(&format!(
            "PREPARE: Dummy shadow OK ({} params) for PG query: {}",
            param_count,
            cstr_prefix(z_sql, 100, "NULL")
        ));
    } else {
        if skip_complex_processing == 0 && contains_icase_ptr(z_sql, "fts4_") {
            if let Ok(sql_str) = unsafe { CStr::from_ptr(z_sql) }.to_str() {
                if let Some(out) = crate::db_interpose_prepare_helpers::simplify_fts_for_sqlite(sql_str) {
                    if let Ok(cs) = CString::new(out) {
                        sql_for_sqlite = cs.as_ptr();
                        cleaned_sql = Some(cs);
                        log_info(&format!("FTS query ORIGINAL: {}", cstr_prefix(z_sql, 500, "NULL")));
                        log_info(&format!(
                            "FTS query SIMPLIFIED: {}",
                            cstr_prefix(sql_for_sqlite, 500, "NULL")
                        ));
                    }
                }
            }
        }

        if skip_complex_processing == 0 && contains_icase_ptr(sql_for_sqlite, "collate icu_root") {
            if let Ok(sql_str) = unsafe { CStr::from_ptr(sql_for_sqlite) }.to_str() {
                if let Some(out) = crate::db_interpose_prepare_helpers::strip_collate_icu_root(sql_str) {
                    if let Ok(cs) = CString::new(out) {
                        cleaned_sql = Some(cs);
                        sql_for_sqlite = cleaned_sql.as_ref().unwrap().as_ptr();
                    }
                }
            }
        }

        if contains_icase_ptr(sql_for_sqlite, "fts4_") || contains_icase_ptr(sql_for_sqlite, " match ") {
            log_info(&format!(
                "FTS query blocked from SQLite (tokenizer not available): {}",
                cstr_prefix(sql_for_sqlite, 100, "NULL")
            ));
            if let Some(prepare) = unsafe { shim_sqlite3_prepare_v2 } {
                rc = unsafe { prepare(
                    db,
                    b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char,
                    -1,
                    pp_stmt,
                    pz_tail,
                ) };
                if rc == SQLITE_OK && !pp_stmt.is_null() && unsafe { !(*pp_stmt).is_null() } {
                    unsafe {
                        pg_note_stmt_prepare(*pp_stmt, b"SELECT 1 WHERE 0\0".as_ptr() as *const c_char);
                    }
                }
                return rc;
            }
        }

        if skip_complex_processing == 0 && !sql_for_sqlite.is_null() {
            if let Ok(sql_str) = unsafe { CStr::from_ptr(sql_for_sqlite) }.to_str() {
                if let Some(out) = crate::db_interpose_prepare_helpers::add_if_not_exists_for_sqlite_ddl(sql_str) {
                    if let Ok(cs) = CString::new(out) {
                        cleaned_sql = Some(cs);
                        sql_for_sqlite = cleaned_sql.as_ref().unwrap().as_ptr();
                        log_info(&format!(
                            "Added IF NOT EXISTS for SQLite DDL: {}",
                            cstr_prefix(sql_for_sqlite, 200, "NULL")
                        ));
                    }
                }
            }
        }

        if let Some(prepare) = unsafe { shim_sqlite3_prepare_v2 } {
            let n = if cleaned_sql.is_some() { -1 } else { n_byte };
            rc = unsafe { prepare(db, sql_for_sqlite, n, pp_stmt, pz_tail) };
        } else {
            log_error("CRITICAL: shim_sqlite3_prepare_v2 not initialized!");
            rc = SQLITE_ERROR;
            if !pp_stmt.is_null() {
                unsafe { *pp_stmt = ptr::null_mut() };
            }
        }

        if rc == SQLITE_OK && !pp_stmt.is_null() && unsafe { !(*pp_stmt).is_null() } {
            unsafe { pg_note_stmt_prepare(*pp_stmt, sql_for_sqlite) };
        } else {
            let sqlite_err = unsafe {
                orig_sqlite3_errmsg
                    .map(|f| f(db))
                    .unwrap_or_else(|| b"unknown\0".as_ptr() as *const c_char)
            };
            let sqlite_errcode = unsafe { orig_sqlite3_errcode.map(|f| f(db)).unwrap_or(-1) };
            log_error(&format!(
                "PREPARE_REAL_SQLITE FAILED: rc={} errcode={} errmsg='{}' sql={}",
                rc,
                sqlite_errcode,
                cstr_to_string_or(sqlite_err, "NULL"),
                cstr_prefix(sql_for_sqlite, 200, "NULL")
            ));
            return rc;
        }
    }

    let pg_conn_for_clear = crate::pg_client::rust_pg_find_connection(db);
    if !pg_conn_for_clear.is_null() {
        unsafe {
            (*pg_conn_for_clear).last_error_code = SQLITE_OK;
            (*pg_conn_for_clear).last_error[0] = 0;
        }
    }

    if !pg_conn.is_null()
        && unsafe { !(*pg_conn).conn.is_null() }
        && unsafe { (*pg_conn).is_pg_active } != 0
        && (is_write || is_read)
        && crate::db_interpose_helpers::rust_is_library_db_path(unsafe { (*pg_conn).db_path.as_ptr() }) != 0
    {
        let pg_stmt = unsafe { pg_stmt_create(pg_conn, z_sql, *pp_stmt) };
        if !pg_stmt.is_null() {
            if crate::pg_config::pg_config_should_skip_sql(z_sql) != 0 {
                unsafe { (*pg_stmt).is_pg = 3 };
            } else {
                unsafe { (*pg_stmt).is_pg = if is_write { 1 } else { 2 } };

                let mut trans = if have_pre_trans {
                    have_pre_trans = false;
                    std::mem::replace(&mut pre_trans, unsafe { std::mem::zeroed() })
                } else {
                    unsafe { sql_translate(z_sql) }
                };

                if trans.success == 0 {
                    log_error(&format!(
                        "Translation failed for SQL: {}. Error: {}",
                        cstr_prefix(z_sql, 200, "NULL"),
                        cstr_to_string_or(trans.error.as_ptr(), "")
                    ));
                }

                unsafe { (*pg_stmt).param_count = trans.param_count };
                unsafe { copy_param_names(pg_stmt, &trans) };

                if trans.success != 0 && !trans.sql.is_null() {
                    let blobs_rewrite = unsafe {
                        rewrite_blobs_schema_migrations(trans.sql, (*pg_conn).db_path.as_ptr())
                    };
                    let effective_sql = if blobs_rewrite.is_null() { trans.sql } else { blobs_rewrite };

                    let aliased =
                        crate::db_interpose_prepare_helpers::alias_collection_sync_aggregates(
                            &cstr_to_string_or(z_sql, ""),
                            &cstr_to_string_or(effective_sql, ""),
                        );
                    let pg_sql_ptr = if let Some(s) = aliased {
                        let cs = CString::new(s).ok();
                        if let Some(cs) = cs.as_ref() {
                            unsafe { libc::strdup(cs.as_ptr()) }
                        } else {
                            unsafe { libc::strdup(effective_sql) }
                        }
                    } else {
                        unsafe { libc::strdup(effective_sql) }
                    };

                    unsafe {
                        (*pg_stmt).pg_sql = pg_sql_ptr;
                    }
                    trace_prepare_pgsql_if_enabled(z_sql, unsafe { (*pg_stmt).pg_sql });

                    unsafe {
                        if !(*pg_stmt).pg_sql.is_null()
                            && contains_ascii_icase(
                                CStr::from_ptr((*pg_stmt).pg_sql).to_bytes(),
                                b"parents.parent_id,count(*)",
                            )
                        {
                            (*pg_stmt).is_count_query = 1;
                        }
                    }

                    if is_write
                        && starts_with_ascii_icase(bytes, b"INSERT")
                        && unsafe { !(*pg_stmt).pg_sql.is_null() }
                        && contains_icase_ptr(unsafe { (*pg_stmt).pg_sql }, "schema_migrations")
                        && !contains_icase_ptr(unsafe { (*pg_stmt).pg_sql }, "ON CONFLICT")
                    {
                        let len = unsafe { libc::strlen((*pg_stmt).pg_sql) };
                        let with_conflict = unsafe { libc::malloc(len + 40) as *mut c_char };
                        if !with_conflict.is_null() {
                            unsafe {
                                libc::snprintf(
                                    with_conflict,
                                    len + 40,
                                    b"%s ON CONFLICT DO NOTHING\0".as_ptr() as *const c_char,
                                    (*pg_stmt).pg_sql,
                                );
                                log_info(&format!(
                                    "SCHEMA_MIGRATIONS: Added ON CONFLICT DO NOTHING: {}",
                                    cstr_prefix(with_conflict, 200, "NULL")
                                ));
                                libc::free((*pg_stmt).pg_sql as *mut c_void);
                                (*pg_stmt).pg_sql = with_conflict;
                            }
                        }
                    }

                    if is_write
                        && starts_with_ascii_icase(bytes, b"INSERT")
                        && unsafe { !(*pg_stmt).pg_sql.is_null() }
                        && !contains_icase_ptr(unsafe { (*pg_stmt).pg_sql }, "RETURNING")
                        && !contains_icase_ptr(unsafe { (*pg_stmt).pg_sql }, "schema_migrations")
                    {
                        let len = unsafe { libc::strlen((*pg_stmt).pg_sql) };
                        let with_returning = unsafe { libc::malloc(len + 20) as *mut c_char };
                        if !with_returning.is_null() {
                            unsafe {
                                libc::snprintf(
                                    with_returning,
                                    len + 20,
                                    b"%s RETURNING id\0".as_ptr() as *const c_char,
                                    (*pg_stmt).pg_sql,
                                );
                                if contains_icase_ptr((*pg_stmt).pg_sql, "play_queue_generators") {
                                    log_info(&format!(
                                        "PREPARE play_queue_generators INSERT with RETURNING: {}",
                                        cstr_prefix(with_returning, 200, "NULL")
                                    ));
                                }
                                libc::free((*pg_stmt).pg_sql as *mut c_void);
                                (*pg_stmt).pg_sql = with_returning;
                            }
                        }
                    }

                    unsafe { apply_prepared_stmt_settings(pg_stmt) };

                    if !blobs_rewrite.is_null() {
                        unsafe { libc::free(blobs_rewrite as *mut c_void) };
                    }
                }

                unsafe { sql_translation_free(&mut trans as *mut SqlTranslation) };
            }

            unsafe { pg_register_stmt(*pp_stmt, pg_stmt) };
        }
    }

    if have_pre_trans {
        unsafe { sql_translation_free(&mut pre_trans as *mut SqlTranslation) };
    }

    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare_v2(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    unsafe { ensure_real_sqlite_loaded() };

    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            if let Some(prepare) = shim_sqlite3_prepare_v2 {
                return prepare(db, z_sql, n_byte, pp_stmt, pz_tail);
            }
            log_error("CRITICAL: shim_sqlite3_prepare_v2 is NULL during recursive call!");
            return SQLITE_ERROR;
        }

        *tls_in_interpose_call_ptr() = 1;
        let result = rust_my_sqlite3_prepare_v2_internal(db, z_sql, n_byte, pp_stmt, pz_tail, 0);
        *tls_in_interpose_call_ptr() = 0;
        result
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    rust_my_sqlite3_prepare_v2(db, z_sql, n_byte, pp_stmt, pz_tail)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare16_v2(
    db: *mut sqlite3,
    z_sql: *const c_void,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_void,
) -> c_int {
    if !z_sql.is_null() {
        let mut utf16_len = 0usize;
        if n_byte < 0 {
            let mut p = z_sql as *const u16;
            unsafe {
                while *p != 0 {
                    p = p.add(1);
                    utf16_len += 1;
                }
            }
            utf16_len *= 2;
        } else {
            utf16_len = n_byte as usize;
        }

        if utf16_len > 0 {
            let max_len = utf16_len * 2 + 1;
            let mut buf = Vec::with_capacity(max_len);
            unsafe {
                let src = z_sql as *const u16;
                let mut i = 0usize;
                while i < utf16_len / 2 && *src.add(i) != 0 {
                    let ch = *src.add(i) as u32;
                    if ch < 0x80 {
                        buf.push(ch as u8);
                    } else if ch < 0x800 {
                        buf.push((0xC0 | (ch >> 6)) as u8);
                        buf.push((0x80 | (ch & 0x3F)) as u8);
                    } else {
                        buf.push((0xE0 | (ch >> 12)) as u8);
                        buf.push((0x80 | ((ch >> 6) & 0x3F)) as u8);
                        buf.push((0x80 | (ch & 0x3F)) as u8);
                    }
                    i += 1;
                }
            }
            if !buf.is_empty() && contains_ascii_icase(&buf, b"collate icu_root") {
                if let Ok(cs) = CString::new(buf) {
                    log_info(&format!(
                        "UTF-16 query with icu_root, routing to UTF-8 handler: {}",
                        cstr_prefix(cs.as_ptr(), 200, "NULL")
                    ));
                    let mut tail8: *const c_char = ptr::null();
                    let rc = rust_my_sqlite3_prepare_v2(db, cs.as_ptr(), -1, pp_stmt, &mut tail8);
                    if !pz_tail.is_null() {
                        unsafe { *pz_tail = ptr::null() };
                    }
                    return rc;
                }
            }
        }
    }

    unsafe {
        if let Some(f) = orig_sqlite3_prepare16_v2 {
            return f(db, z_sql, n_byte, pp_stmt, pz_tail);
        }
    }
    unsafe { sqlite3_prepare16_v2(db, z_sql, n_byte, pp_stmt, pz_tail) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_prepare_v3(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    prep_flags: u32,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    if !z_sql.is_null() && contains_icase_ptr(z_sql, "metadata_items") {
        log_info(&format!(
            "PREPARE_V3 metadata_items query: {}",
            cstr_prefix(z_sql, 200, "NULL")
        ));
    }
    let _ = prep_flags;
    rust_my_sqlite3_prepare_v2(db, z_sql, n_byte, pp_stmt, pz_tail)
}
