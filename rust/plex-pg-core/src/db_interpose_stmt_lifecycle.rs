use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use crate::db_interpose_conn_utils::{cstr_to_string_or, log_debug, log_error, log_info, PthreadMutexGuard};
use crate::ffi_types::{sqlite3_stmt, PgStmt, MAX_PARAMS, PARAM_BUF_LEN};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

const FINALIZED_RING_SIZE: usize = 2048;
const FINALIZED_RECENT_MS: u64 = 2000;
const PREPARED_RING_SIZE: usize = 4096;

static FINALIZED_RING_IDX: AtomicU32 = AtomicU32::new(0);
static PREPARED_RING_IDX: AtomicU32 = AtomicU32::new(0);
static CLEAR_BINDINGS_COUNTER: AtomicU64 = AtomicU64::new(0);

static SKIP_CLEAR_BINDINGS_CACHED: AtomicI32 = AtomicI32::new(-1);
static TRACE_CLEAR_BINDINGS_CACHED: AtomicI32 = AtomicI32::new(-1);

#[repr(C)]
#[derive(Copy, Clone)]
struct FinalizedEntry {
    stmt: *mut sqlite3_stmt,
    ts_ns: u64,
    tid: u64,
    is_pg: c_int,
    sql: [c_char; 256],
}

impl FinalizedEntry {
    const fn empty() -> Self {
        Self {
            stmt: ptr::null_mut(),
            ts_ns: 0,
            tid: 0,
            is_pg: 0,
            sql: [0; 256],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
struct PreparedEntry {
    stmt: *mut sqlite3_stmt,
    ts_ns: u64,
    tid: u64,
    sql: [c_char; 256],
}

impl PreparedEntry {
    const fn empty() -> Self {
        Self {
            stmt: ptr::null_mut(),
            ts_ns: 0,
            tid: 0,
            sql: [0; 256],
        }
    }
}

static mut FINALIZED_RING: [FinalizedEntry; FINALIZED_RING_SIZE] =
    [FinalizedEntry::empty(); FINALIZED_RING_SIZE];
static mut PREPARED_RING: [PreparedEntry; PREPARED_RING_SIZE] =
    [PreparedEntry::empty(); PREPARED_RING_SIZE];

extern "C" {
    static mut orig_sqlite3_reset: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_finalize: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_clear_bindings: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char>;

    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);

    fn pg_find_any_stmt(stmt: *mut sqlite3_stmt) -> *mut PgStmt;
    fn pg_find_stmt(stmt: *mut sqlite3_stmt) -> *mut PgStmt;
    fn pg_find_cached_stmt(stmt: *mut sqlite3_stmt) -> *mut PgStmt;
    fn pg_clear_cached_stmt(stmt: *mut sqlite3_stmt);
    fn pg_unregister_stmt(stmt: *mut sqlite3_stmt);
    fn pg_stmt_unref(stmt: *mut PgStmt);
    fn pg_stmt_clear_result(stmt: *mut PgStmt);
}

fn skip_clear_bindings_on_finalized() -> bool {
    let cached = SKIP_CLEAR_BINDINGS_CACHED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let name = b"PLEX_PG_SKIP_CLEAR_BINDINGS_FINALIZED\0";
    let val = unsafe {
        let env = libc::getenv(name.as_ptr() as *const c_char);
        if env.is_null() {
            1
        } else {
            crate::db_interpose_helpers::rust_env_truthy(env)
        }
    };
    let flag = if val != 0 { 1 } else { 0 };
    SKIP_CLEAR_BINDINGS_CACHED.store(flag, Ordering::Relaxed);
    flag == 1
}

fn trace_clear_bindings_enabled() -> bool {
    let cached = TRACE_CLEAR_BINDINGS_CACHED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let name = b"PLEX_PG_TRACE_CLEAR_BINDINGS\0";
    let val = unsafe {
        let env = libc::getenv(name.as_ptr() as *const c_char);
        crate::db_interpose_helpers::rust_env_truthy(env)
    };
    let flag = if val != 0 { 1 } else { 0 };
    TRACE_CLEAR_BINDINGS_CACHED.store(flag, Ordering::Relaxed);
    flag == 1
}

fn now_monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1_000_000_000u64 + (ts.tv_nsec as u64)
}

unsafe fn write_sql_buf(buf: &mut [c_char; 256], sql: *const c_char) {
    buf[0] = 0;
    if sql.is_null() || *sql == 0 {
        return;
    }
    libc::strncpy(buf.as_mut_ptr(), sql, buf.len() - 1);
    buf[buf.len() - 1] = 0;
}

unsafe fn remember_finalized_stmt(stmt: *mut sqlite3_stmt, sql: *const c_char, is_pg: c_int) {
    if stmt.is_null() {
        return;
    }
    let idx = FINALIZED_RING_IDX.fetch_add(1, Ordering::Relaxed) as usize % FINALIZED_RING_SIZE;
    let entry = finalized_ring_ptr().add(idx);
    (*entry).stmt = stmt;
    (*entry).ts_ns = now_monotonic_ns();
    (*entry).tid = libc::pthread_self() as u64;
    (*entry).is_pg = is_pg;
    write_sql_buf(&mut (*entry).sql, sql);
}

unsafe fn remember_prepared_stmt(stmt: *mut sqlite3_stmt, sql: *const c_char) {
    if stmt.is_null() {
        return;
    }
    let idx = PREPARED_RING_IDX.fetch_add(1, Ordering::Relaxed) as usize % PREPARED_RING_SIZE;
    let entry = prepared_ring_ptr().add(idx);
    (*entry).stmt = stmt;
    (*entry).ts_ns = now_monotonic_ns();
    (*entry).tid = libc::pthread_self() as u64;
    write_sql_buf(&mut (*entry).sql, sql);
}

unsafe fn is_prepared_stmt(stmt: *mut sqlite3_stmt) -> bool {
    if stmt.is_null() {
        return false;
    }
    let base = prepared_ring_ptr();
    for i in 0..PREPARED_RING_SIZE {
        if (*base.add(i)).stmt == stmt {
            return true;
        }
    }
    false
}

unsafe fn clear_prepared_stmt(stmt: *mut sqlite3_stmt) {
    if stmt.is_null() {
        return;
    }
    let base = prepared_ring_ptr();
    for i in 0..PREPARED_RING_SIZE {
        if (*base.add(i)).stmt == stmt {
            *base.add(i) = PreparedEntry::empty();
            return;
        }
    }
}

unsafe fn clear_finalized_entry(stmt: *mut sqlite3_stmt) {
    if stmt.is_null() {
        return;
    }
    let base = finalized_ring_ptr();
    for i in 0..FINALIZED_RING_SIZE {
        if (*base.add(i)).stmt == stmt {
            *base.add(i) = FinalizedEntry::empty();
            return;
        }
    }
}

unsafe fn find_finalized_entry(stmt: *mut sqlite3_stmt) -> Option<FinalizedEntry> {
    if stmt.is_null() {
        return None;
    }
    let base = finalized_ring_ptr();
    for i in 0..FINALIZED_RING_SIZE {
        if (*base.add(i)).stmt == stmt {
            return Some(*base.add(i));
        }
    }
    None
}

unsafe fn log_clear_bindings_anomaly(reason: &str, stmt: *mut sqlite3_stmt) {
    if !trace_clear_bindings_enabled() {
        return;
    }
    let n = CLEAR_BINDINGS_COUNTER.fetch_add(1, Ordering::Relaxed);
    if n >= 5 && (n % 1000) != 0 {
        return;
    }

    if let Some(entry) = find_finalized_entry(stmt) {
        if entry.ts_ns != 0 {
            let now_ns = now_monotonic_ns();
            let age_ms = if now_ns > entry.ts_ns {
                (now_ns - entry.ts_ns) / 1_000_000
            } else {
                0
            };
            let sql = if entry.sql[0] == 0 {
                "NULL".to_string()
            } else {
                cstr_to_string_or(entry.sql.as_ptr(), "NULL")
            };
            log_error(&format!(
                "CLEAR_BINDINGS anomaly: {} stmt={:p} age_ms={} finalize_tid=0x{:x} is_pg={} sql={}",
                reason,
                stmt,
                age_ms,
                entry.tid,
                entry.is_pg,
                sql
            ));
        } else {
            log_error(&format!(
                "CLEAR_BINDINGS anomaly: {} stmt={:p} (no finalize metadata)",
                reason, stmt
            ));
        }
    } else {
        log_error(&format!(
            "CLEAR_BINDINGS anomaly: {} stmt={:p} (no finalize metadata)",
            reason, stmt
        ));
    }

    if let Ok(cs) = CString::new("CLEAR_BINDINGS anomaly") {
        platform_print_backtrace(cs.as_ptr(), 2);
    }
}

unsafe fn is_recently_finalized_stmt(stmt: *mut sqlite3_stmt) -> bool {
    let Some(entry) = find_finalized_entry(stmt) else {
        return false;
    };
    if entry.ts_ns == 0 {
        return false;
    }
    let now_ns = now_monotonic_ns();
    let age_ms = if now_ns > entry.ts_ns {
        (now_ns - entry.ts_ns) / 1_000_000
    } else {
        0
    };
    age_ms <= FINALIZED_RECENT_MS
}

unsafe fn is_preallocated_buffer(stmt: *mut PgStmt, idx: usize) -> bool {
    if stmt.is_null() || idx >= MAX_PARAMS {
        return false;
    }
    let val = (*stmt).param_values[idx];
    if val.is_null() {
        return false;
    }
    let val_addr = val as usize;
    let base = (*stmt).param_buffers[idx].as_ptr() as usize;
    val_addr >= base && val_addr < base + PARAM_BUF_LEN
}

unsafe fn reset_pg_stmt_locked(p_stmt: *mut sqlite3_stmt, stmt: *mut PgStmt) -> c_int {
    let _guard = PthreadMutexGuard::lock(&mut (*stmt).mutex as *mut _);
    (*stmt).in_step.store(0, Ordering::Relaxed);

    for i in 0..MAX_PARAMS {
        if !(*stmt).param_values[i].is_null() && !is_preallocated_buffer(stmt, i) {
            libc::free((*stmt).param_values[i] as *mut c_void);
            (*stmt).param_values[i] = ptr::null_mut();
        }
    }
    pg_stmt_clear_result(stmt);

    if (*stmt).is_pg != 2 {
        return orig_sqlite3_reset.map(|f| f(p_stmt)).unwrap_or(SQLITE_ERROR);
    }
    SQLITE_OK
}

#[no_mangle]
pub extern "C" fn rust_pg_note_stmt_prepare(p_stmt: *mut sqlite3_stmt, sql: *const c_char) {
    unsafe {
        remember_prepared_stmt(p_stmt, sql);
        clear_finalized_entry(p_stmt);
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_reset(p_stmt: *mut sqlite3_stmt) -> c_int {
    let pg_stmt = unsafe { pg_find_any_stmt(p_stmt) };
    if !pg_stmt.is_null() {
        return unsafe { reset_pg_stmt_locked(p_stmt, pg_stmt) };
    }

    let cached = unsafe { pg_find_cached_stmt(p_stmt) };
    if !cached.is_null() {
        return unsafe { reset_pg_stmt_locked(p_stmt, cached) };
    }

    unsafe { orig_sqlite3_reset.map(|f| f(p_stmt)).unwrap_or(SQLITE_ERROR) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_finalize(p_stmt: *mut sqlite3_stmt) -> c_int {
    unsafe {
        if skip_clear_bindings_on_finalized() && is_recently_finalized_stmt(p_stmt) {
            log_clear_bindings_anomaly("finalize on recently finalized", p_stmt);
            clear_prepared_stmt(p_stmt);
            return SQLITE_OK;
        }
        if !is_prepared_stmt(p_stmt) {
            log_clear_bindings_anomaly("finalize on unknown stmt", p_stmt);
            clear_prepared_stmt(p_stmt);
            return SQLITE_OK;
        }

        let mut is_pg_only = 0;
        let mut is_pg_value = 0;
        let mut final_sql: *const c_char = ptr::null();

        let pg_stmt = pg_find_stmt(p_stmt);
        if !pg_stmt.is_null() {
            is_pg_value = (*pg_stmt).is_pg;
            is_pg_only = if (*pg_stmt).is_pg == 2 { 1 } else { 0 };
            final_sql = if !(*pg_stmt).pg_sql.is_null() {
                (*pg_stmt).pg_sql
            } else {
                (*pg_stmt).sql
            };

            let cached = pg_find_cached_stmt(p_stmt);
            if cached == pg_stmt {
                log_debug("finalize: stmt in both global and TLS, clearing TLS ref");
                pg_clear_cached_stmt(p_stmt);
            } else if !cached.is_null() {
                log_info("finalize: different pg_stmt in global vs TLS for same sqlite_stmt (cross-thread re-prepare)");
                pg_clear_cached_stmt(p_stmt);
            }

            pg_unregister_stmt(p_stmt);
            pg_stmt_unref(pg_stmt);
        } else {
            let cached = pg_find_cached_stmt(p_stmt);
            if !cached.is_null() {
                is_pg_value = (*cached).is_pg;
                is_pg_only = if (*cached).is_pg == 2 { 1 } else { 0 };
                if final_sql.is_null() {
                    final_sql = if !(*cached).pg_sql.is_null() {
                        (*cached).pg_sql
                    } else {
                        (*cached).sql
                    };
                }
                log_debug(&format!(
                    "finalize: stmt only in TLS (ref_count={}), clearing",
                    (*cached).ref_count.load(Ordering::Relaxed)
                ));
                pg_clear_cached_stmt(p_stmt);
                pg_stmt_unref(cached);
            }
        }

        if final_sql.is_null() {
            if let Some(f) = orig_sqlite3_sql {
                final_sql = f(p_stmt);
            }
        }

        let mut rc = SQLITE_OK;
        if is_pg_only == 0 {
            rc = orig_sqlite3_finalize.map(|f| f(p_stmt)).unwrap_or(SQLITE_ERROR);
        }
        clear_prepared_stmt(p_stmt);
        remember_finalized_stmt(p_stmt, final_sql, is_pg_value);
        rc
    }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_clear_bindings(p_stmt: *mut sqlite3_stmt) -> c_int {
    unsafe {
        if skip_clear_bindings_on_finalized() && is_recently_finalized_stmt(p_stmt) {
            log_clear_bindings_anomaly("recently finalized", p_stmt);
            return SQLITE_OK;
        }

        let pg_stmt = pg_find_stmt(p_stmt);
        if pg_stmt.is_null() {
            log_clear_bindings_anomaly("stmt not registered", p_stmt);
        }

        if !pg_stmt.is_null() {
            let _guard = PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _);
            for i in 0..MAX_PARAMS {
                if !(*pg_stmt).param_values[i].is_null() && !is_preallocated_buffer(pg_stmt, i) {
                    libc::free((*pg_stmt).param_values[i] as *mut c_void);
                    (*pg_stmt).param_values[i] = ptr::null_mut();
                }
            }
            if (*pg_stmt).is_pg == 0 {
                return orig_sqlite3_clear_bindings.map(|f| f(p_stmt)).unwrap_or(SQLITE_ERROR);
            }
            return SQLITE_OK;
        }

        orig_sqlite3_clear_bindings.map(|f| f(p_stmt)).unwrap_or(SQLITE_ERROR)
    }
}
unsafe fn finalized_ring_ptr() -> *mut FinalizedEntry {
    ptr::addr_of_mut!(FINALIZED_RING) as *mut FinalizedEntry
}

unsafe fn prepared_ring_ptr() -> *mut PreparedEntry {
    ptr::addr_of_mut!(PREPARED_RING) as *mut PreparedEntry
}
