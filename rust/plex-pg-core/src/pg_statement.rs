/// Module: pg_statement
///
/// Statement registry, TLS cached statements, reference counting, and
/// pure helper functions for the PostgreSQL shim.
///
/// **Phase 3 migration**: This module now owns the statement registry
/// (hash map), TLS cached statement list, and fake sqlite3_value pool.
/// The C file `pg_statement.c` becomes a thin shim.
///
/// ## Memory safety fixes (vs. C original)
///
/// - **HIGH #2 fix**: Refcount ABA race in `atomic_fetch_sub` — Rust uses
///   `fetch_sub` with `Ordering::AcqRel` plus explicit underflow detection
///   that restores the count to 0 instead of going negative.
///
/// - **HIGH #3 fix**: TLS destructor frees statement still in global registry.
///   The Rust TLS `Drop` calls `unref` which only triggers `pg_stmt_free`
///   when the last reference is gone. The registry holds its own reference.
///
/// ## Design
///
/// - Statement registry: `RwLock<HashMap<usize, usize>>` for O(1) lookup
///   (key = sqlite3_stmt* as usize, value = pg_stmt_t* as usize)
/// - TLS cached stmts: `thread_local!` with `Drop` that unrefs all entries
/// - pg_value pool: lock-free circular buffer with `AtomicU32`
/// - All existing pure helpers (OID mapping, upsert, metadata ID) unchanged
///
/// ## FFI exports (new)
///
///   - `rust_stmt_registry_init`       — initialize registry
///   - `rust_stmt_registry_cleanup`    — clear all entries
///   - `rust_stmt_register`            — insert sqlite_stmt → pg_stmt mapping
///   - `rust_stmt_unregister`          — remove mapping
///   - `rust_stmt_find`                — lookup by sqlite_stmt
///   - `rust_stmt_find_any`            — lookup in registry + TLS cache
///   - `rust_stmt_is_ours`             — check if pg_stmt pointer is registered
///   - `rust_stmt_ref`                 — increment ref_count
///   - `rust_stmt_unref`               — decrement ref_count, free at 0
///   - `rust_cached_stmt_register`     — add to TLS cache (with ref)
///   - `rust_cached_stmt_find`         — lookup in TLS cache
///   - `rust_cached_stmt_clear`        — remove from TLS cache (with unref)
///   - `rust_cached_stmt_clear_weak`   — remove from TLS cache (no unref)
///   - `rust_create_column_value`      — allocate fake sqlite3_value
///   - `rust_is_our_value`             — check magic on sqlite3_value
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Once, RwLock};

use crate::db_interpose_conn_utils::{log_debug, log_error, log_info, PthreadMutexGuard};
use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::ffi_types::{sqlite3_stmt, sqlite3_value, PgConnection, PgStmt, MAX_PARAMS, PARAM_BUF_LEN};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Max cached statements per thread (must match C MAX_CACHED_STMTS_PER_THREAD).
const MAX_CACHED_STMTS_PER_THREAD: usize = 64;

/// Fake sqlite3_value pool size (must match C MAX_PG_VALUES).
const MAX_PG_VALUES: usize = 4096;

/// Magic number to identify our fake sqlite3_value (must match C PG_VALUE_MAGIC).
const PG_VALUE_MAGIC: u32 = 0x50475641; // "PGVA"

// SQLite type constants (must match C)
const SQLITE_INTEGER: i32 = 1;
const SQLITE_FLOAT: i32 = 2;
const SQLITE_TEXT: i32 = 3;
const SQLITE_BLOB: i32 = 4;
#[allow(dead_code)]
const SQLITE_NULL: i32 = 5;

const DECLTYPE_CASE_NONE: i32 = 0;
const DECLTYPE_CASE_NULL: i32 = 1;
const DECLTYPE_CASE_DT_INTEGER_8: i32 = 2;

// ─── Static decltype byte strings (null-terminated) ──────────────────────────

static DECLTYPE_INTEGER: &[u8] = b"INTEGER\0";
static DECLTYPE_BIGINT: &[u8] = b"BIGINT\0";
static DECLTYPE_REAL: &[u8] = b"REAL\0";
static DECLTYPE_BLOB: &[u8] = b"BLOB\0";
static DECLTYPE_TEXT: &[u8] = b"TEXT\0";

// ─── ON CONFLICT clause ──────────────────────────────────────────────────────

const ON_CONFLICT_CLAUSE: &str = " ON CONFLICT (account_id, guid) DO UPDATE SET \
rating = COALESCE(EXCLUDED.rating, plex.metadata_item_settings.rating), \
view_offset = EXCLUDED.view_offset, \
view_count = CASE WHEN plex.metadata_item_settings.view_count > 0 AND EXCLUDED.view_count = 0 \
THEN 0 ELSE GREATEST(EXCLUDED.view_count, plex.metadata_item_settings.view_count, 1) END, \
last_viewed_at = CASE WHEN plex.metadata_item_settings.view_count > 0 AND EXCLUDED.view_count = 0 \
THEN NULL ELSE COALESCE(EXCLUDED.last_viewed_at, EXTRACT(EPOCH FROM NOW())::bigint) END, \
updated_at = COALESCE(EXCLUDED.updated_at, EXTRACT(EPOCH FROM NOW())::bigint), \
skip_count = EXCLUDED.skip_count, \
last_skipped_at = EXCLUDED.last_skipped_at, \
changed_at = COALESCE(EXCLUDED.changed_at, EXTRACT(EPOCH FROM NOW())::bigint), \
extra_data = COALESCE(EXCLUDED.extra_data, plex.metadata_item_settings.extra_data), \
last_rated_at = COALESCE(EXCLUDED.last_rated_at, plex.metadata_item_settings.last_rated_at) \
RETURNING id";

// ─── Statement Registry (global, RwLock-protected) ───────────────────────────

/// Global statement registry: maps sqlite3_stmt* → pg_stmt_t*.
///
/// Uses `usize` as the key/value to avoid carrying raw pointer types through
/// the RwLock. The C shim casts to/from `void*`.
///
/// A secondary reverse map tracks pg_stmt_t* → sqlite3_stmt* for the
/// `pg_is_our_stmt` lookup (which searches by pg_stmt, not sqlite_stmt).
static REGISTRY: std::sync::LazyLock<RwLock<StmtRegistry>> =
    std::sync::LazyLock::new(|| RwLock::new(StmtRegistry::new()));
static STMT_INIT: Once = Once::new();

struct StmtRegistry {
    /// Forward map: sqlite3_stmt* → pg_stmt_t*
    forward: HashMap<usize, usize>,
    /// Reverse map: pg_stmt_t* → sqlite3_stmt* (for is_our_stmt lookup)
    reverse: HashMap<usize, usize>,
}

impl StmtRegistry {
    fn new() -> Self {
        Self {
            forward: HashMap::with_capacity(512),
            reverse: HashMap::with_capacity(512),
        }
    }

    fn register(&mut self, sqlite_stmt: usize, pg_stmt: usize) {
        // Remove old reverse mapping if this sqlite_stmt was already registered
        if let Some(old_pg) = self.forward.insert(sqlite_stmt, pg_stmt) {
            if old_pg != pg_stmt {
                self.reverse.remove(&old_pg);
            }
        }
        self.reverse.insert(pg_stmt, sqlite_stmt);
    }

    fn unregister(&mut self, sqlite_stmt: usize) {
        if let Some(pg_stmt) = self.forward.remove(&sqlite_stmt) {
            self.reverse.remove(&pg_stmt);
        }
    }

    fn find(&self, sqlite_stmt: usize) -> Option<usize> {
        self.forward.get(&sqlite_stmt).copied()
    }

    fn is_ours(&self, pg_stmt: usize) -> bool {
        self.reverse.contains_key(&pg_stmt)
    }

    fn clear(&mut self) {
        self.forward.clear();
        self.reverse.clear();
    }

    fn len(&self) -> usize {
        self.forward.len()
    }
}

// ─── TLS Cached Statements ──────────────────────────────────────────────────

/// Entry in the per-thread cached statement list.
struct CachedStmtEntry {
    sqlite_stmt: usize, // sqlite3_stmt* as usize
    pg_stmt: usize,     // pg_stmt_t* as usize
}

/// Per-thread cached statement list with FIFO eviction.
///
/// When the list is full (MAX_CACHED_STMTS_PER_THREAD), the oldest entry
/// is evicted. Evicted entries get their ref_count decremented.
struct ThreadCachedStmts {
    entries: Vec<CachedStmtEntry>,
}

impl ThreadCachedStmts {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(MAX_CACHED_STMTS_PER_THREAD),
        }
    }

    /// Register a cached statement. Increments ref_count on the pg_stmt.
    /// If the sqlite_stmt is already cached, replaces the pg_stmt (unrefs old).
    fn register(&mut self, sqlite_stmt: usize, pg_stmt: usize) {
        // Check if already registered — replace
        for entry in &mut self.entries {
            if entry.sqlite_stmt == sqlite_stmt {
                let old = entry.pg_stmt;
                if old != pg_stmt {
                    stmt_unref_ptr(old);
                }
                stmt_ref_ptr(pg_stmt);
                entry.pg_stmt = pg_stmt;
                return;
            }
        }

        // New entry — increment ref
        stmt_ref_ptr(pg_stmt);

        if self.entries.len() < MAX_CACHED_STMTS_PER_THREAD {
            self.entries.push(CachedStmtEntry {
                sqlite_stmt,
                pg_stmt,
            });
        } else {
            // Evict oldest (index 0)
            let old = self.entries[0].pg_stmt;
            stmt_unref_ptr(old);
            self.entries.remove(0);
            self.entries.push(CachedStmtEntry {
                sqlite_stmt,
                pg_stmt,
            });
        }
    }

    /// Find a cached statement by sqlite_stmt.
    fn find(&self, sqlite_stmt: usize) -> Option<usize> {
        for entry in &self.entries {
            if entry.sqlite_stmt == sqlite_stmt {
                return Some(entry.pg_stmt);
            }
        }
        None
    }

    /// Remove a cached statement and unref it.
    fn clear(&mut self, sqlite_stmt: usize) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.sqlite_stmt == sqlite_stmt)
        {
            let old_pg_stmt = self.entries[pos].pg_stmt;
            self.entries.remove(pos);
            stmt_unref_ptr(old_pg_stmt);
        }
    }

    /// Remove a cached statement WITHOUT unreffing (weak clear).
    /// Used by finalize() because the global registry owns the reference.
    fn clear_weak(&mut self, sqlite_stmt: usize) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.sqlite_stmt == sqlite_stmt)
        {
            self.entries.remove(pos);
        }
    }

    /// Get all pg_stmt pointers (for TLS destructor to unref).
    fn drain_all(&mut self) -> Vec<usize> {
        self.entries.drain(..).map(|e| e.pg_stmt).collect()
    }
}

impl Drop for ThreadCachedStmts {
    fn drop(&mut self) {
        if stmt_cache_disabled() {
            self.entries.clear();
            return;
        }
        for entry in self.entries.drain(..) {
            stmt_unref_ptr(entry.pg_stmt);
        }
    }
}

thread_local! {
    static TLS_CACHED_STMTS: RefCell<Option<ThreadCachedStmts>> = const { RefCell::new(None) };
}

/// Get or create the TLS cached statements.
fn with_tls_cache<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut ThreadCachedStmts) -> R,
{
    TLS_CACHED_STMTS
        .try_with(|cell| {
        let mut borrow = cell.borrow_mut();
        let cache = borrow.get_or_insert_with(ThreadCachedStmts::new);
        Some(f(cache))
    })
        .ok()
        .flatten()
}

// ─── Fake sqlite3_value pool ─────────────────────────────────────────────────

/// Matches the C `pg_value_t` struct layout exactly.
#[repr(C)]
pub struct PgValue {
    pub magic: u32,
    pub stmt: usize, // pg_stmt_t* as usize
    pub col_idx: i32,
    pub sqlite_type: i32,
}

/// Lock-free pool of fake sqlite3_value entries.
/// Uses atomic wrapping index — entries are recycled after MAX_PG_VALUES allocations.
static PG_VALUE_IDX: AtomicU32 = AtomicU32::new(0);

// We can't use a static Vec, so we use a fixed-size array via LazyLock.
// Each PgValue is small (20 bytes) and the pool is only 4096 entries.
static PG_VALUES: std::sync::LazyLock<Vec<std::sync::Mutex<PgValue>>> =
    std::sync::LazyLock::new(|| {
        let mut v = Vec::with_capacity(MAX_PG_VALUES);
        for _ in 0..MAX_PG_VALUES {
            v.push(std::sync::Mutex::new(PgValue {
                magic: 0,
                stmt: 0,
                col_idx: 0,
                sqlite_type: 0,
            }));
        }
        v
    });

// ─── Internal pure helpers (unchanged from Phase 2) ──────────────────────────

/// Map a PostgreSQL OID to an SQLite type constant.
pub(crate) fn oid_to_sqlite_type(oid: u32) -> i32 {
    match oid {
        16 | 20 | 21 | 23 | 26 => SQLITE_INTEGER,
        700 | 701 | 1700 => SQLITE_FLOAT,
        17 => SQLITE_BLOB,
        _ => SQLITE_TEXT,
    }
}

/// Map a PostgreSQL OID to an SQLite declared-type string.
pub(crate) fn oid_to_sqlite_decltype(oid: u32) -> &'static CStr {
    let bytes: &'static [u8] = match oid {
        16 | 21 | 23 | 26 => DECLTYPE_INTEGER,
        20 => DECLTYPE_BIGINT,
        700 | 701 | 1700 => DECLTYPE_REAL,
        17 => DECLTYPE_BLOB,
        _ => DECLTYPE_TEXT,
    };
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

/// Convert a `metadata_item_settings` INSERT into an upsert.
pub(crate) fn convert_metadata_settings_upsert(sql: &str) -> Option<String> {
    let lower = sql.to_lowercase();
    if !lower.contains("insert into") {
        return None;
    }
    if !lower.contains("metadata_item_settings") {
        return None;
    }
    if lower.contains("on conflict") {
        return None;
    }
    if lower.contains("returning") {
        return None;
    }
    Some(format!("{}{}", sql, ON_CONFLICT_CLAUSE))
}

/// Extract the metadata item ID from a `play_queue_generators` INSERT SQL.
pub(crate) fn extract_metadata_id(sql: &str) -> i64 {
    let lower = sql.to_lowercase();
    if !lower.contains("play_queue_generators") {
        return 0;
    }
    if !lower.contains("insert") {
        return 0;
    }

    let pat_encoded = "%2Fmetadata%2F";
    let pat_plain = "/metadata/";

    let after = if let Some(i) = sql.find(pat_encoded) {
        &sql[i + pat_encoded.len()..]
    } else if let Some(i) = sql.find(pat_plain) {
        &sql[i + pat_plain.len()..]
    } else {
        return 0;
    };

    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    digits.parse::<i64>().unwrap_or(0)
}

static LEAK_STMTS: std::sync::LazyLock<AtomicI32> =
    std::sync::LazyLock::new(|| AtomicI32::new(-1));
static DISABLE_STMT_CACHE: std::sync::LazyLock<AtomicI32> =
    std::sync::LazyLock::new(|| AtomicI32::new(-1));

fn env_truthy(name: &str) -> bool {
    crate::env_utils::env_truthy_str(name)
}

fn leak_enabled() -> bool {
    let cached = LEAK_STMTS.load(Ordering::Relaxed);
    if cached >= 0 {
        return cached != 0;
    }
    let enabled = env_truthy("PLEX_PG_LEAK_STMTS");
    LEAK_STMTS.store(enabled as i32, Ordering::Relaxed);
    enabled
}

fn stmt_cache_disabled() -> bool {
    let cached = DISABLE_STMT_CACHE.load(Ordering::Relaxed);
    if cached >= 0 {
        return cached != 0;
    }
    let enabled = env_truthy("PLEX_PG_DISABLE_STMT_CACHE");
    DISABLE_STMT_CACHE.store(enabled as i32, Ordering::Relaxed);
    enabled
}

fn stmt_ref_ptr(pg_stmt: usize) {
    if pg_stmt == 0 {
        return;
    }
    rust_stmt_ref(pg_stmt as *mut PgStmt);
}

fn stmt_unref_ptr(pg_stmt: usize) {
    if pg_stmt == 0 {
        return;
    }
    rust_stmt_unref(pg_stmt as *mut PgStmt);
}

extern "C" {
    fn pg_pool_validate_connection(conn: *mut PgConnection) -> c_int;
}

const PMT_STMT_SWEEP_EXTRA_FREE: i32 = 6;

unsafe fn is_preallocated_buffer(stmt: &PgStmt, idx: usize) -> bool {
    let val = stmt.param_values[idx] as usize;
    if val == 0 {
        return false;
    }
    let buf_ptr = stmt.param_buffers[idx].as_ptr() as usize;
    val >= buf_ptr && val < buf_ptr + PARAM_BUF_LEN
}

#[no_mangle]
pub extern "C" fn rust_stmt_create(
    conn: *mut PgConnection,
    sql: *const c_char,
    shadow_stmt: *mut crate::ffi_types::sqlite3_stmt,
) -> *mut PgStmt {
    unsafe {
        let stmt_ptr = libc::calloc(1, std::mem::size_of::<PgStmt>()) as *mut PgStmt;
        if stmt_ptr.is_null() {
            log_error("pg_stmt_create: calloc failed");
            return std::ptr::null_mut();
        }

        let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
        if libc::pthread_mutexattr_init(&mut attr as *mut _) != 0 {
            log_error("pg_stmt_create: pthread_mutexattr_init failed");
            libc::free(stmt_ptr as *mut c_void);
            return std::ptr::null_mut();
        }
        libc::pthread_mutexattr_settype(&mut attr as *mut _, libc::PTHREAD_MUTEX_RECURSIVE);
        libc::pthread_mutex_init(&mut (*stmt_ptr).mutex as *mut _, &attr as *const _);
        libc::pthread_mutexattr_destroy(&mut attr as *mut _);

        (*stmt_ptr).ref_count.store(1, Ordering::Release);
        (*stmt_ptr).conn = conn;
        (*stmt_ptr).shadow_stmt = shadow_stmt;
        (*stmt_ptr).sql = if sql.is_null() {
            std::ptr::null_mut()
        } else {
            libc::strdup(sql)
        };
        (*stmt_ptr).current_row = -1;
        (*stmt_ptr).cached_row = -1;
        (*stmt_ptr).decoded_blob_row = -1;
        (*stmt_ptr).write_executed = 0;
        (*stmt_ptr).read_done = 0;

        stmt_ptr
    }
}

#[no_mangle]
pub extern "C" fn rust_stmt_free(stmt_ptr: *mut PgStmt) {
    if stmt_ptr.is_null() {
        return;
    }
    unsafe {
        let stmt = &mut *stmt_ptr;

        let ref_count = stmt.ref_count.load(Ordering::Acquire);
        if ref_count != 0 {
            let sql = if stmt.sql.is_null() {
                "NULL"
            } else {
                cstr_to_str_or_empty(stmt.sql)
            };
            log_error(&format!(
                "pg_stmt_free: WARNING ref_count={} (expected 0) for stmt={:p} sql={:.50}",
                ref_count, stmt_ptr, sql
            ));
            if ref_count > 0 {
                log_error(&format!(
                    "pg_stmt_free: ABORT - ref_count={} not freeing to prevent use-after-free",
                    ref_count
                ));
                return;
            }
        }

        if stmt.streaming_mode != 0 && !stmt.streaming_conn.is_null() {
            if pg_pool_validate_connection(stmt.streaming_conn) == 0 {
                log_error(&format!(
                    "pg_stmt_free: streaming_conn invalid, skipping cancel/drain (stmt={:p})",
                    stmt_ptr
                ));
                stmt.streaming_mode = 0;
                stmt.streaming_conn = std::ptr::null_mut();
            } else {
                let sconn = stmt.streaming_conn;
                let _conn_guard = PthreadMutexGuard::lock(&mut (*sconn).mutex as *mut _);
                if !(*sconn).conn.is_null() {
                    let cancel = crate::libpq_helpers::rust_pq_get_cancel((*sconn).conn);
                    if !cancel.is_null() {
                        let mut errbuf = [0 as c_char; 256];
                        if crate::libpq_helpers::rust_pq_cancel(
                            cancel,
                            errbuf.as_mut_ptr(),
                            errbuf.len() as c_int,
                        ) == 0
                        {
                            let err = cstr_to_str_or_empty(errbuf.as_ptr());
                            log_error(&format!("pg_stmt_free: PQcancel failed: {}", err));
                        }
                        crate::libpq_helpers::rust_pq_free_cancel(cancel);
                    }
                    let mut drain_count = 0;
                    loop {
                        let drain = crate::libpq_helpers::rust_pq_get_result((*sconn).conn);
                        if drain.is_null() {
                            break;
                        }
                        drain_count += 1;
                        crate::libpq_helpers::rust_pq_clear(drain);
                        if drain_count > 1000 {
                            log_info(&format!(
                                "pg_stmt_free: drain after cancel exceeded 1000 on {:p}",
                                sconn
                            ));
                            break;
                        }
                    }
                    if drain_count > 0 {
                        log_debug(&format!(
                            "pg_stmt_free: drained {} results after cancel",
                            drain_count
                        ));
                    }
                }
                stmt.streaming_mode = 0;
                (*sconn)
                    .streaming_active
                    .store(0, Ordering::Release);
                stmt.streaming_conn = std::ptr::null_mut();
            }
        }

        log_debug(&format!(
            "pg_stmt_free: START stmt={:p} sql={:p} pg_sql={:p}",
            stmt_ptr, stmt.sql, stmt.pg_sql
        ));

        let pg_sql_is_separate = !stmt.pg_sql.is_null() && stmt.pg_sql != stmt.sql;

        if !stmt.sql.is_null() {
            let sql = if stmt.sql.is_null() {
                "NULL"
            } else {
                cstr_to_str_or_empty(stmt.sql)
            };
            log_debug(&format!(
                "pg_stmt_free: freeing sql={:p} ({:.50})",
                stmt.sql, sql
            ));
            libc::free(stmt.sql as *mut c_void);
            stmt.sql = std::ptr::null_mut();
        }
        if pg_sql_is_separate && !stmt.pg_sql.is_null() {
            let sql = cstr_to_str_or_empty(stmt.pg_sql);
            log_debug(&format!(
                "pg_stmt_free: freeing pg_sql={:p} ({:.50})",
                stmt.pg_sql, sql
            ));
            libc::free(stmt.pg_sql as *mut c_void);
            stmt.pg_sql = std::ptr::null_mut();
        }
        if !stmt.result.is_null() {
            log_debug(&format!("pg_stmt_free: PQclear result={:p}", stmt.result));
            crate::libpq_helpers::rust_pq_clear(stmt.result);
            stmt.result = std::ptr::null_mut();
        }

        let mut safe_param_count = stmt.param_count;
        if safe_param_count < 0 {
            safe_param_count = 0;
        }
        if safe_param_count as usize > MAX_PARAMS {
            safe_param_count = MAX_PARAMS as c_int;
        }

        for i in 0..MAX_PARAMS {
            let val = stmt.param_values[i];
            if !val.is_null() && !is_preallocated_buffer(stmt, i) {
                log_debug(&format!(
                    "pg_stmt_free: freeing param_values[{}]={:p}",
                    i, val
                ));
                libc::free(val as *mut c_void);
                stmt.param_values[i] = std::ptr::null_mut();
                if (i as c_int) >= safe_param_count
                    && crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0
                {
                    crate::pg_mem_telemetry::rust_mem_telemetry_add(
                        PMT_STMT_SWEEP_EXTRA_FREE,
                        0,
                        1,
                    );
                }
            }
        }

        if !stmt.param_names.is_null() {
            log_debug(&format!(
                "pg_stmt_free: freeing param_names={:p} (array of {})",
                stmt.param_names, safe_param_count
            ));
            let count = safe_param_count.max(0) as usize;
            for i in 0..count {
                let slot = stmt.param_names.add(i);
                if !(*slot).is_null() {
                    let name = cstr_to_str_or_empty(*slot);
                    log_debug(&format!(
                        "pg_stmt_free: freeing param_names[{}]={:p} ({:.30})",
                        i, *slot, name
                    ));
                    libc::free(*slot as *mut c_void);
                    *slot = std::ptr::null_mut();
                }
            }
            log_debug(&format!(
                "pg_stmt_free: freeing param_names array at {:p}",
                stmt.param_names
            ));
            libc::free(stmt.param_names as *mut c_void);
            stmt.param_names = std::ptr::null_mut();
        }

        for i in 0..MAX_PARAMS {
            let blob = stmt.decoded_blobs[i];
            if !blob.is_null() {
                log_debug(&format!(
                    "pg_stmt_free: freeing decoded_blobs[{}]={:p}",
                    i, blob
                ));
                libc::free(blob as *mut c_void);
                stmt.decoded_blobs[i] = std::ptr::null_mut();
            }
        }

        for i in 0..MAX_PARAMS {
            let text = stmt.cached_text[i];
            if !text.is_null() {
                log_debug(&format!(
                    "pg_stmt_free: freeing cached_text[{}]={:p}",
                    i, text
                ));
                libc::free(text as *mut c_void);
                stmt.cached_text[i] = std::ptr::null_mut();
            }
            let blob = stmt.cached_blob[i];
            if !blob.is_null() {
                log_debug(&format!(
                    "pg_stmt_free: freeing cached_blob[{}]={:p}",
                    i, blob
                ));
                libc::free(blob as *mut c_void);
                stmt.cached_blob[i] = std::ptr::null_mut();
            }
        }

        for i in 0..MAX_PARAMS {
            let name = stmt.col_table_names[i];
            if !name.is_null() {
                libc::free(name as *mut c_void);
                stmt.col_table_names[i] = std::ptr::null_mut();
            }
        }

        if !stmt.col_names.is_null() {
            let count = if stmt.num_col_names > 0 {
                stmt.num_col_names as usize
            } else {
                0
            };
            for i in 0..count {
                let slot = stmt.col_names.add(i);
                if !(*slot).is_null() {
                    libc::free(*slot as *mut c_void);
                }
            }
            libc::free(stmt.col_names as *mut c_void);
            stmt.col_names = std::ptr::null_mut();
            stmt.num_col_names = 0;
        }

        log_debug(&format!(
            "pg_stmt_free: destroying mutex and freeing stmt={:p}",
            stmt_ptr
        ));
        libc::pthread_mutex_destroy(&mut stmt.mutex as *mut _);
        libc::free(stmt_ptr as *mut c_void);
        log_debug("pg_stmt_free: DONE");
    }
}

#[no_mangle]
pub extern "C" fn rust_stmt_clear_result(stmt_ptr: *mut PgStmt) {
    if stmt_ptr.is_null() {
        return;
    }
    unsafe {
        let stmt = &mut *stmt_ptr;

        if stmt.streaming_mode != 0 && !stmt.streaming_conn.is_null() {
            if pg_pool_validate_connection(stmt.streaming_conn) == 0 {
                log_error(&format!(
                    "pg_stmt_clear_result: streaming_conn invalid, skipping cancel/drain (stmt={:p})",
                    stmt_ptr
                ));
                stmt.streaming_mode = 0;
                stmt.streaming_conn = std::ptr::null_mut();
            } else {
                let sconn = stmt.streaming_conn;
                let _conn_guard = PthreadMutexGuard::lock(&mut (*sconn).mutex as *mut _);
                if !(*sconn).conn.is_null() {
                    let cancel = crate::libpq_helpers::rust_pq_get_cancel((*sconn).conn);
                    if !cancel.is_null() {
                        let mut errbuf = [0 as c_char; 256];
                        if crate::libpq_helpers::rust_pq_cancel(
                            cancel,
                            errbuf.as_mut_ptr(),
                            errbuf.len() as c_int,
                        ) == 0
                        {
                            let err = cstr_to_str_or_empty(errbuf.as_ptr());
                            log_error(&format!(
                                "pg_stmt_clear_result: PQcancel failed: {}",
                                err
                            ));
                        }
                        crate::libpq_helpers::rust_pq_free_cancel(cancel);
                    }
                    let mut drain_count = 0;
                    loop {
                        let drain = crate::libpq_helpers::rust_pq_get_result((*sconn).conn);
                        if drain.is_null() {
                            break;
                        }
                        drain_count += 1;
                        crate::libpq_helpers::rust_pq_clear(drain);
                        if drain_count > 1000 {
                            log_info(&format!(
                                "pg_stmt_clear_result: drain after cancel exceeded 1000 on {:p}",
                                sconn
                            ));
                            break;
                        }
                    }
                    if drain_count > 0 {
                        let sql = if stmt.sql.is_null() {
                            "NULL"
                        } else {
                            cstr_to_str_or_empty(stmt.sql)
                        };
                        log_debug(&format!(
                            "pg_stmt_clear_result: drained {} results after cancel (sql={:.60})",
                            drain_count, sql
                        ));
                    }
                }
                stmt.streaming_mode = 0;
                (*sconn)
                    .streaming_active
                    .store(0, Ordering::Release);
                stmt.streaming_conn = std::ptr::null_mut();
            }
        }

        if !stmt.result.is_null() {
            crate::libpq_helpers::rust_pq_clear(stmt.result);
            stmt.result = std::ptr::null_mut();
        }
        if !stmt.cached_result.is_null() {
            crate::pg_query_cache::rust_query_cache_release(stmt.cached_result);
            stmt.cached_result = std::ptr::null_mut();
        }
        stmt.result_conn = std::ptr::null_mut();
        stmt.metadata_only_result = 0;
        stmt.current_row = -1;
        stmt.num_rows = 0;
        stmt.num_cols = 0;
        stmt.write_executed = 0;
        stmt.read_done = 0;

        for i in 0..MAX_PARAMS {
            let blob = stmt.decoded_blobs[i];
            if !blob.is_null() {
                libc::free(blob as *mut c_void);
                stmt.decoded_blobs[i] = std::ptr::null_mut();
                stmt.decoded_blob_lens[i] = 0;
            }
        }
        stmt.decoded_blob_row = -1;

        for i in 0..MAX_PARAMS {
            let text = stmt.cached_text[i];
            if !text.is_null() {
                libc::free(text as *mut c_void);
                stmt.cached_text[i] = std::ptr::null_mut();
            }
            let blob = stmt.cached_blob[i];
            if !blob.is_null() {
                libc::free(blob as *mut c_void);
                stmt.cached_blob[i] = std::ptr::null_mut();
                stmt.cached_blob_len[i] = 0;
            }
        }
        stmt.cached_row = -1;

        if !stmt.col_names.is_null() {
            let count = if stmt.num_col_names > 0 {
                stmt.num_col_names as usize
            } else {
                0
            };
            for i in 0..count {
                let slot = stmt.col_names.add(i);
                if !(*slot).is_null() {
                    libc::free(*slot as *mut c_void);
                }
            }
            libc::free(stmt.col_names as *mut c_void);
            stmt.col_names = std::ptr::null_mut();
            stmt.num_col_names = 0;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Public C FFI functions — existing (unchanged)
// ═══════════════════════════════════════════════════════════════════════════════

#[no_mangle]
pub extern "C" fn rust_oid_to_sqlite_type(oid: u32) -> i32 {
    oid_to_sqlite_type(oid)
}

#[no_mangle]
pub extern "C" fn rust_oid_to_sqlite_decltype(oid: u32) -> *const c_char {
    oid_to_sqlite_decltype(oid).as_ptr()
}

#[no_mangle]
pub extern "C" fn rust_decltype_special_case(
    oid: u32,
    col_name: *const c_char,
    pg_sql: *const c_char,
    table_oid: u32,
) -> i32 {
    let col = unsafe { cstr_to_str_or_empty(col_name) };
    let sql = unsafe { cstr_to_str_or_empty(pg_sql) };

    if oid == 20 && !col.is_empty() {
        if col.contains("_at") || col.contains("timestamp") || col.contains("time") {
            return DECLTYPE_CASE_DT_INTEGER_8;
        }
        if col == "greatest" && sql.contains("metadata_items.changed_at") {
            return DECLTYPE_CASE_DT_INTEGER_8;
        }
    }

    if table_oid == 0 {
        return DECLTYPE_CASE_NULL;
    }

    DECLTYPE_CASE_NONE
}

#[no_mangle]
pub extern "C" fn rust_convert_metadata_settings_upsert(sql: *const c_char) -> *mut c_char {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    if s.is_empty() {
        return std::ptr::null_mut();
    }
    match convert_metadata_settings_upsert(s) {
        Some(result) => CString::new(result)
            .map(|cs| cs.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn rust_extract_metadata_id(sql: *const c_char) -> i64 {
    let s = unsafe { cstr_to_str_or_empty(sql) };
    if s.is_empty() {
        return 0;
    }
    extract_metadata_id(s)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Public C FFI functions — new Phase 3 (statement registry + TLS cache)
// ═══════════════════════════════════════════════════════════════════════════════

#[no_mangle]
pub extern "C" fn rust_stmt_ref(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }
    unsafe {
        let stmt = &*pg_stmt;
        stmt.ref_count.fetch_add(1, Ordering::AcqRel);
    }
}

#[no_mangle]
pub extern "C" fn rust_stmt_unref(pg_stmt: *mut PgStmt) {
    if pg_stmt.is_null() {
        return;
    }
    let old = unsafe {
        let stmt = &*pg_stmt;
        stmt.ref_count.fetch_sub(1, Ordering::AcqRel)
    };
    let new = old - 1;
    let sql = unsafe {
        let stmt = &*pg_stmt;
        if stmt.sql.is_null() {
            "NULL"
        } else {
            if stmt.sql.is_null() {
                "NULL"
            } else {
                cstr_to_str_or_empty(stmt.sql)
            }
        }
    };
    log_debug(&format!(
        "pg_stmt_unref: stmt={:p} old_ref={} new_ref={} sql={:.40}",
        pg_stmt, old, new, sql
    ));

    if old <= 0 {
        log_error(&format!(
            "pg_stmt_unref: CRITICAL BUG - ref_count was {} before decrement! stmt={:p} sql={:.40}",
            old, pg_stmt, sql
        ));
        log_error("pg_stmt_unref: This indicates double-unref or missing ref. RESTORING to prevent negative.");
        unsafe {
            let stmt = &*pg_stmt;
            stmt.ref_count.store(0, Ordering::Release);
        }
        return;
    }

    if old == 1 {
        if leak_enabled() {
            log_error(&format!(
                "pg_stmt_unref: leak enabled via PLEX_PG_LEAK_STMTS, skipping free stmt={:p} sql={:.40}",
                pg_stmt, sql
            ));
            unsafe {
                let stmt = &*pg_stmt;
                stmt.ref_count.store(1, Ordering::Release);
            }
            return;
        }
        log_debug(&format!(
            "pg_stmt_unref: last reference, freeing stmt={:p}",
            pg_stmt
        ));
        rust_stmt_free(pg_stmt);
    }
}

/// Initialize the statement registry.
#[no_mangle]
pub extern "C" fn rust_stmt_registry_init() {
    STMT_INIT.call_once(|| {
        // Force LazyLock initialization
        let _reg = REGISTRY.read().unwrap();
        log_debug("pg_statement registry initialized (Rust HashMap)");
    });
}

/// Clear all entries from the registry.
/// Each pg_stmt_t gets unref'd.
#[no_mangle]
pub extern "C" fn rust_stmt_registry_cleanup() {
    let mut reg = REGISTRY.write().unwrap();
    // Collect all pg_stmt pointers before clearing
    let pg_stmts: Vec<usize> = reg.forward.values().copied().collect();
    reg.clear();
    drop(reg); // Release write lock before calling unref
    for pg_stmt in pg_stmts {
        stmt_unref_ptr(pg_stmt);
    }
}

/// Register a sqlite3_stmt → pg_stmt_t mapping.
///
/// # Safety
/// Both pointers must be valid. The pg_stmt_t must remain valid until
/// `rust_stmt_unregister` is called.
#[no_mangle]
pub extern "C" fn rust_stmt_register(sqlite_stmt: usize, pg_stmt: usize) {
    if sqlite_stmt == 0 || pg_stmt == 0 {
        return;
    }
    let mut reg = REGISTRY.write().unwrap();
    reg.register(sqlite_stmt, pg_stmt);
}

/// Remove a sqlite3_stmt → pg_stmt_t mapping.
#[no_mangle]
pub extern "C" fn rust_stmt_unregister(sqlite_stmt: usize) {
    if sqlite_stmt == 0 {
        return;
    }
    let mut reg = REGISTRY.write().unwrap();
    reg.unregister(sqlite_stmt);
}

/// Look up pg_stmt_t by sqlite3_stmt pointer.
/// Returns 0 if not found.
#[no_mangle]
pub extern "C" fn rust_stmt_find(sqlite_stmt: usize) -> usize {
    if sqlite_stmt == 0 {
        return 0;
    }
    let reg = REGISTRY.read().unwrap();
    reg.find(sqlite_stmt).unwrap_or(0)
}

/// Look up pg_stmt_t by sqlite3_stmt pointer — first in registry, then TLS cache.
/// Returns 0 if not found anywhere.
#[no_mangle]
pub extern "C" fn rust_stmt_find_any(sqlite_stmt: usize) -> usize {
    if sqlite_stmt == 0 {
        return 0;
    }

    // Fast path: registry lookup
    {
        let reg = REGISTRY.read().unwrap();
        if let Some(pg_stmt) = reg.find(sqlite_stmt) {
            return pg_stmt;
        }
    }

    // Fallback: TLS cache lookup (if enabled)
    if stmt_cache_disabled() {
        return 0;
    }
    with_tls_cache(|cache| cache.find(sqlite_stmt).unwrap_or(0)).unwrap_or(0)
}

/// Check if a pg_stmt_t pointer is registered.
#[no_mangle]
pub extern "C" fn rust_stmt_is_ours(pg_stmt: usize) -> i32 {
    if pg_stmt == 0 {
        return 0;
    }
    let reg = REGISTRY.read().unwrap();
    if reg.is_ours(pg_stmt) {
        1
    } else {
        0
    }
}

/// Get the current number of registered statements.
#[no_mangle]
pub extern "C" fn rust_stmt_registry_count() -> usize {
    let reg = REGISTRY.read().unwrap();
    reg.len()
}

// ─── TLS Cached Statements FFI ──────────────────────────────────────────────

/// Register a cached statement in the TLS cache.
/// Increments ref_count.
#[no_mangle]
pub extern "C" fn rust_cached_stmt_register(sqlite_stmt: usize, pg_stmt: usize) {
    if sqlite_stmt == 0 || pg_stmt == 0 {
        return;
    }
    if stmt_cache_disabled() {
        return;
    }
    with_tls_cache(|cache| {
        cache.register(sqlite_stmt, pg_stmt);
    });
}

/// Find a cached statement in the TLS cache.
/// Returns 0 if not found.
#[no_mangle]
pub extern "C" fn rust_cached_stmt_find(sqlite_stmt: usize) -> usize {
    if sqlite_stmt == 0 {
        return 0;
    }
    if stmt_cache_disabled() {
        return 0;
    }
    with_tls_cache(|cache| cache.find(sqlite_stmt).unwrap_or(0)).unwrap_or(0)
}

/// Remove a cached statement from the TLS cache with unref.
#[no_mangle]
pub extern "C" fn rust_cached_stmt_clear(sqlite_stmt: usize) {
    if sqlite_stmt == 0 {
        return;
    }
    if stmt_cache_disabled() {
        return;
    }
    with_tls_cache(|cache| {
        cache.clear(sqlite_stmt);
    });
}

/// Remove a cached statement from the TLS cache WITHOUT unref (weak clear).
/// Used by finalize() because the global registry owns the reference.
#[no_mangle]
pub extern "C" fn rust_cached_stmt_clear_weak(sqlite_stmt: usize) {
    if sqlite_stmt == 0 {
        return;
    }
    if stmt_cache_disabled() {
        return;
    }
    with_tls_cache(|cache| {
        cache.clear_weak(sqlite_stmt);
    });
}

/// Drain all TLS cached statements (for thread exit cleanup).
/// Returns the pg_stmt pointers that need unreffing. The C shim calls this
/// from the TLS destructor.
///
/// # Safety
/// The returned array is heap-allocated and must be freed by the caller.
/// `count_out` must point to a valid i32.
#[no_mangle]
pub extern "C" fn rust_cached_stmt_drain_all(count_out: *mut i32) -> *mut usize {
    if stmt_cache_disabled() {
        if !count_out.is_null() {
            unsafe {
                *count_out = 0;
            }
        }
        return std::ptr::null_mut();
    }
    let stmts = with_tls_cache(|cache| cache.drain_all());
    let stmts = stmts.unwrap_or_default();
    let count = stmts.len();

    if !count_out.is_null() {
        unsafe {
            *count_out = count as i32;
        }
    }

    if stmts.is_empty() {
        return std::ptr::null_mut();
    }

    // Allocate via libc so C can free it
    unsafe {
        let ptr = libc::malloc(count * std::mem::size_of::<usize>()) as *mut usize;
        if ptr.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(stmts.as_ptr(), ptr, count);
        ptr
    }
}

// ─── Fake sqlite3_value FFI ─────────────────────────────────────────────────

/// Allocate a fake sqlite3_value from the lock-free pool.
///
/// Returns a pointer to a `PgValue` that can be cast to `sqlite3_value*`
/// by the C code. The entry is filled with the given stmt pointer, column
/// index, and sqlite type.
///
/// The pool wraps around after MAX_PG_VALUES allocations, recycling entries.
/// This matches the C implementation exactly.
#[no_mangle]
pub extern "C" fn rust_create_column_value(
    stmt: usize,
    col_idx: i32,
    sqlite_type: i32,
) -> *mut PgValue {
    let slot = PG_VALUE_IDX.fetch_add(1, Ordering::Relaxed) as usize % MAX_PG_VALUES;
    let pool = &PG_VALUES[slot];
    let mut pv = pool.lock().unwrap();
    pv.magic = PG_VALUE_MAGIC;
    pv.stmt = stmt;
    pv.col_idx = col_idx;
    pv.sqlite_type = sqlite_type;
    &mut *pv as *mut PgValue
}

/// Check if a pointer is a fake sqlite3_value created by us.
///
/// # Safety
/// `val` must be a valid pointer to at least 4 bytes (the magic field).
#[no_mangle]
pub extern "C" fn rust_is_our_value(val: *const PgValue) -> i32 {
    if val.is_null() {
        return 0;
    }
    // Safety: we only read the first 4 bytes (magic field)
    let magic = unsafe { (*val).magic };
    if magic == PG_VALUE_MAGIC {
        1
    } else {
        0
    }
}

// ─── C ABI wrappers (pg_statement.c replacement) ─────────────────────────────

#[no_mangle]
pub extern "C" fn pg_statement_init() {
    rust_stmt_registry_init();
}

#[no_mangle]
pub extern "C" fn pg_statement_cleanup() {
    rust_stmt_registry_cleanup();
}

#[no_mangle]
pub extern "C" fn pg_register_stmt(sqlite_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) {
    rust_stmt_register(sqlite_stmt as usize, pg_stmt as usize);
}

#[no_mangle]
pub extern "C" fn pg_unregister_stmt(sqlite_stmt: *mut sqlite3_stmt) {
    rust_stmt_unregister(sqlite_stmt as usize);
}

#[no_mangle]
pub extern "C" fn pg_find_stmt(stmt: *mut sqlite3_stmt) -> *mut PgStmt {
    rust_stmt_find(stmt as usize) as *mut PgStmt
}

#[no_mangle]
pub extern "C" fn pg_find_any_stmt(stmt: *mut sqlite3_stmt) -> *mut PgStmt {
    rust_stmt_find_any(stmt as usize) as *mut PgStmt
}

#[no_mangle]
pub extern "C" fn pg_is_our_stmt(ptr: *mut c_void) -> c_int {
    rust_stmt_is_ours(ptr as usize)
}

#[no_mangle]
pub extern "C" fn pg_register_cached_stmt(sqlite_stmt: *mut sqlite3_stmt, pg_stmt: *mut PgStmt) {
    rust_cached_stmt_register(sqlite_stmt as usize, pg_stmt as usize);
}

#[no_mangle]
pub extern "C" fn pg_find_cached_stmt(sqlite_stmt: *mut sqlite3_stmt) -> *mut PgStmt {
    rust_cached_stmt_find(sqlite_stmt as usize) as *mut PgStmt
}

#[no_mangle]
pub extern "C" fn pg_clear_cached_stmt(sqlite_stmt: *mut sqlite3_stmt) {
    rust_cached_stmt_clear(sqlite_stmt as usize);
}

#[no_mangle]
pub extern "C" fn pg_clear_cached_stmt_weak(sqlite_stmt: *mut sqlite3_stmt) {
    rust_cached_stmt_clear_weak(sqlite_stmt as usize);
}

#[no_mangle]
pub extern "C" fn pg_stmt_create(
    conn: *mut PgConnection,
    sql: *const c_char,
    shadow_stmt: *mut sqlite3_stmt,
) -> *mut PgStmt {
    rust_stmt_create(conn, sql, shadow_stmt)
}

#[no_mangle]
pub extern "C" fn pg_stmt_free(stmt: *mut PgStmt) {
    rust_stmt_free(stmt);
}

#[no_mangle]
pub extern "C" fn pg_stmt_ref(stmt: *mut PgStmt) {
    rust_stmt_ref(stmt);
}

#[no_mangle]
pub extern "C" fn pg_stmt_unref(stmt: *mut PgStmt) {
    rust_stmt_unref(stmt);
}

#[no_mangle]
pub extern "C" fn pg_stmt_clear_result(stmt: *mut PgStmt) {
    rust_stmt_clear_result(stmt);
}

#[no_mangle]
pub extern "C" fn pg_oid_to_sqlite_type(oid: u32) -> c_int {
    rust_oid_to_sqlite_type(oid)
}

#[no_mangle]
pub extern "C" fn pg_oid_to_sqlite_decltype(oid: u32) -> *const c_char {
    rust_oid_to_sqlite_decltype(oid)
}

#[no_mangle]
pub extern "C" fn pg_decltype_special_case(
    oid: u32,
    col_name: *const c_char,
    pg_sql: *const c_char,
    table_oid: u32,
) -> c_int {
    rust_decltype_special_case(oid, col_name, pg_sql, table_oid)
}

#[no_mangle]
pub extern "C" fn pg_create_column_value(pg_stmt: *mut PgStmt, col_idx: c_int) -> *mut sqlite3_value {
    if pg_stmt.is_null() || unsafe { (*pg_stmt).result.is_null() } {
        return rust_create_column_value(pg_stmt as usize, col_idx, SQLITE_NULL) as *mut sqlite3_value;
    }
    let sqlite_type = unsafe {
        crate::db_interpose_helpers::rust_pg_create_column_value(
            (*pg_stmt).result,
            (*pg_stmt).current_row,
            (*pg_stmt).num_rows,
            col_idx,
        )
    };
    rust_create_column_value(pg_stmt as usize, col_idx, sqlite_type) as *mut sqlite3_value
}

#[no_mangle]
pub extern "C" fn pg_is_our_value(val: *mut sqlite3_value) -> c_int {
    rust_is_our_value(val as *const PgValue)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Unit tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn make_stmt(sql: &str) -> *mut PgStmt {
        let csql = CString::new(sql).unwrap();
        let stmt = rust_stmt_create(std::ptr::null_mut(), csql.as_ptr(), std::ptr::null_mut());
        assert!(!stmt.is_null());
        stmt
    }

    fn ref_count(stmt: *mut PgStmt) -> i32 {
        unsafe { (*stmt).ref_count.load(Ordering::Relaxed) }
    }

    // ── Registry: basic operations (unit tests on StmtRegistry directly) ────

    #[test]
    fn registry_register_and_find() {
        let mut reg = StmtRegistry::new();
        reg.register(0x1000, 0x2000);
        assert_eq!(reg.find(0x1000), Some(0x2000));
    }

    #[test]
    fn registry_find_missing_returns_none() {
        let reg = StmtRegistry::new();
        assert_eq!(reg.find(0xDEAD), None);
    }

    #[test]
    fn registry_unregister_removes_both_maps() {
        let mut reg = StmtRegistry::new();
        reg.register(0x3000, 0x4000);
        assert!(reg.is_ours(0x4000));
        reg.unregister(0x3000);
        assert_eq!(reg.find(0x3000), None);
        assert!(!reg.is_ours(0x4000));
    }

    #[test]
    fn registry_is_ours_true_for_registered() {
        let mut reg = StmtRegistry::new();
        reg.register(0x5000, 0x6000);
        assert!(reg.is_ours(0x6000));
    }

    #[test]
    fn registry_is_ours_false_for_unregistered() {
        let reg = StmtRegistry::new();
        assert!(!reg.is_ours(0xBEEF));
    }

    #[test]
    fn registry_replace_existing_mapping() {
        let mut reg = StmtRegistry::new();
        reg.register(0x7000, 0x8000);
        assert_eq!(reg.find(0x7000), Some(0x8000));
        reg.register(0x7000, 0x9000);
        assert_eq!(reg.find(0x7000), Some(0x9000));
        // Old reverse mapping should be cleaned up
        assert!(!reg.is_ours(0x8000));
        assert!(reg.is_ours(0x9000));
    }

    #[test]
    fn registry_clear_empties_all() {
        let mut reg = StmtRegistry::new();
        reg.register(0xA000, 0xB000);
        reg.register(0xC000, 0xD000);
        assert_eq!(reg.len(), 2);
        reg.clear();
        assert_eq!(reg.len(), 0);
        assert_eq!(reg.find(0xA000), None);
    }

    // ── Registry: concurrent access (via global REGISTRY) ────────────────────

    #[test]
    fn registry_concurrent_readers() {
        // Use unique keys to avoid test interference
        let key = 0xFF_E000_usize;
        let val = 0xFF_F000_usize;

        // Set up test data using the global REGISTRY
        REGISTRY
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .register(key, val);

        // Multiple concurrent readers
        let handles: Vec<_> = (0..8)
            .map(move |_| {
                std::thread::spawn(move || {
                    let reg = REGISTRY.read().unwrap_or_else(|e| e.into_inner());
                    reg.find(key)
                })
            })
            .collect();

        for h in handles {
            assert_eq!(h.join().unwrap(), Some(val));
        }

        // Cleanup
        REGISTRY
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .unregister(key);
    }

    // ── TLS cached stmts: basic operations ───────────────────────────────────

    #[test]
    fn tls_cache_register_and_find() {
        let mut cache = ThreadCachedStmts::new();
        let stmt = make_stmt("SELECT 1");
        cache.register(0x100, stmt as usize);
        assert_eq!(cache.find(0x100), Some(stmt as usize));
        assert_eq!(ref_count(stmt), 2);

        cache.clear(0x100);
        assert_eq!(cache.find(0x100), None);
        assert_eq!(ref_count(stmt), 1);

        rust_stmt_unref(stmt);
    }

    #[test]
    fn tls_cache_find_missing_returns_none() {
        let cache = ThreadCachedStmts::new();
        assert_eq!(cache.find(0x999), None);
    }

    #[test]
    fn tls_cache_replace_unrefs_old() {
        let mut cache = ThreadCachedStmts::new();
        let stmt_a = make_stmt("SELECT 1");
        let stmt_b = make_stmt("SELECT 2");

        // Register first stmt
        cache.register(0x100, stmt_a as usize);
        assert_eq!(ref_count(stmt_a), 2);

        // Replace with different pg_stmt
        cache.register(0x100, stmt_b as usize);

        // Old should be unreffed, new should be reffed
        assert_eq!(ref_count(stmt_a), 1);
        assert_eq!(ref_count(stmt_b), 2);
        assert_eq!(cache.find(0x100), Some(stmt_b as usize));

        cache.clear(0x100);
        assert_eq!(cache.find(0x100), None);
        assert_eq!(ref_count(stmt_b), 1);

        rust_stmt_unref(stmt_a);
        rust_stmt_unref(stmt_b);
    }

    #[test]
    fn tls_cache_clear_unrefs() {
        let mut cache = ThreadCachedStmts::new();
        let stmt = make_stmt("SELECT 1");
        cache.register(0x100, stmt as usize);
        assert_eq!(ref_count(stmt), 2);

        cache.clear(0x100);
        assert_eq!(ref_count(stmt), 1);
        assert_eq!(cache.find(0x100), None);

        rust_stmt_unref(stmt);
    }

    #[test]
    fn tls_cache_clear_weak_does_not_unref() {
        let mut cache = ThreadCachedStmts::new();
        let stmt = make_stmt("SELECT 1");
        cache.register(0x100, stmt as usize);
        assert_eq!(ref_count(stmt), 2);

        cache.clear_weak(0x100);
        // ref_count should NOT change — weak clear
        assert_eq!(ref_count(stmt), 2);
        assert_eq!(cache.find(0x100), None);

        rust_stmt_unref(stmt);
        rust_stmt_unref(stmt);
    }

    #[test]
    fn tls_cache_fifo_eviction() {
        let mut cache = ThreadCachedStmts::new();
        let mut stmts: Vec<*mut PgStmt> = Vec::new();

        // Fill cache to max
        for i in 0..MAX_CACHED_STMTS_PER_THREAD {
            let stmt = make_stmt("SELECT 1");
            cache.register(0x1000 + i, stmt as usize);
            stmts.push(stmt);
        }
        assert_eq!(cache.entries.len(), MAX_CACHED_STMTS_PER_THREAD);

        // One more — should evict oldest (0x1000 → 0x2000)
        let extra = make_stmt("SELECT 2");
        cache.register(0x9999, extra as usize);
        stmts.push(extra);
        assert_eq!(cache.find(0x1000), None); // evicted
        assert_eq!(cache.find(0x9999), Some(extra as usize)); // new entry

        // Drop cached refs, then drop initial refs.
        let cached = cache.drain_all();
        for pg_stmt in cached {
            rust_stmt_unref(pg_stmt as *mut PgStmt);
        }
        for stmt in stmts {
            rust_stmt_unref(stmt);
        }
    }

    #[test]
    fn tls_cache_drain_all_returns_all_pg_stmts() {
        let mut cache = ThreadCachedStmts::new();
        let stmt_a = make_stmt("SELECT 1");
        let stmt_b = make_stmt("SELECT 2");
        cache.register(0x100, stmt_a as usize);
        cache.register(0x300, stmt_b as usize);

        let drained = cache.drain_all();
        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&(stmt_a as usize)));
        assert!(drained.contains(&(stmt_b as usize)));
        assert!(cache.entries.is_empty());

        for pg_stmt in drained {
            rust_stmt_unref(pg_stmt as *mut PgStmt);
        }
        rust_stmt_unref(stmt_a);
        rust_stmt_unref(stmt_b);
    }

    // ── TLS cache: thread isolation ──────────────────────────────────────────

    #[test]
    fn tls_cache_is_thread_local() {
        let stmt = make_stmt("SELECT 1");

        // Register on this thread
        with_tls_cache(|cache| {
            cache.register(0xAAAA, stmt as usize);
        });

        // Other thread should not see it
        let handle = std::thread::spawn(|| {
            with_tls_cache(|cache| cache.find(0xAAAA).is_none()).unwrap_or(true)
        });
        assert!(handle.join().unwrap());

        // Cleanup
        with_tls_cache(|cache| {
            cache.clear(0xAAAA);
        });

        rust_stmt_unref(stmt);
    }

    // ── pg_value pool ────────────────────────────────────────────────────────

    #[test]
    fn pg_value_create_sets_magic() {
        let ptr = rust_create_column_value(0x1234, 0, SQLITE_INTEGER);
        assert!(!ptr.is_null());
        let val = unsafe { &*ptr };
        assert_eq!(val.magic, PG_VALUE_MAGIC);
        assert_eq!(val.stmt, 0x1234);
        assert_eq!(val.col_idx, 0);
        assert_eq!(val.sqlite_type, SQLITE_INTEGER);
    }

    #[test]
    fn pg_value_is_our_value_true() {
        let ptr = rust_create_column_value(0x5678, 3, SQLITE_TEXT);
        assert_eq!(rust_is_our_value(ptr), 1);
    }

    #[test]
    fn pg_value_is_our_value_null_false() {
        assert_eq!(rust_is_our_value(std::ptr::null()), 0);
    }

    #[test]
    fn pg_value_pool_wraps_around() {
        // Allocate more than MAX_PG_VALUES
        for i in 0..MAX_PG_VALUES + 10 {
            let ptr = rust_create_column_value(i, 0, SQLITE_INTEGER);
            assert!(!ptr.is_null());
        }
        // Should not crash — wraps around
    }

    // ── FFI functions: registry ──────────────────────────────────────────────

    #[test]
    fn ffi_register_find_unregister() {
        let s = 0x10000_usize;
        let p = 0x20000_usize;

        rust_stmt_register(s, p);
        assert_eq!(rust_stmt_find(s), p);
        assert_eq!(rust_stmt_is_ours(p), 1);

        rust_stmt_unregister(s);
        assert_eq!(rust_stmt_find(s), 0);
        assert_eq!(rust_stmt_is_ours(p), 0);
    }

    #[test]
    fn ffi_find_null_returns_zero() {
        assert_eq!(rust_stmt_find(0), 0);
    }

    #[test]
    fn ffi_find_any_checks_registry_first() {
        let s = 0x30000_usize;
        let p = 0x40000_usize;

        rust_stmt_register(s, p);
        assert_eq!(rust_stmt_find_any(s), p);
        rust_stmt_unregister(s);
    }

    #[test]
    fn ffi_find_any_falls_back_to_tls() {
        let s = 0x50000_usize;
        let stmt = make_stmt("SELECT 1");

        // Not in registry
        assert_eq!(rust_stmt_find(s), 0);

        // Add to TLS cache
        with_tls_cache(|cache| {
            cache.register(s, stmt as usize);
        });

        assert_eq!(rust_stmt_find_any(s), stmt as usize);

        // Cleanup
        with_tls_cache(|cache| {
            cache.clear(s);
        });

        rust_stmt_unref(stmt);
    }

    // ── Existing tests (OID mapping, upsert, metadata ID) ────────────────────

    #[test]
    fn type_bool_is_integer() {
        assert_eq!(oid_to_sqlite_type(16), SQLITE_INTEGER);
    }

    #[test]
    fn type_int8_is_integer() {
        assert_eq!(oid_to_sqlite_type(20), SQLITE_INTEGER);
    }

    #[test]
    fn type_int2_is_integer() {
        assert_eq!(oid_to_sqlite_type(21), SQLITE_INTEGER);
    }

    #[test]
    fn type_int4_is_integer() {
        assert_eq!(oid_to_sqlite_type(23), SQLITE_INTEGER);
    }

    #[test]
    fn type_oid_is_integer() {
        assert_eq!(oid_to_sqlite_type(26), SQLITE_INTEGER);
    }

    #[test]
    fn type_float4_is_float() {
        assert_eq!(oid_to_sqlite_type(700), SQLITE_FLOAT);
    }

    #[test]
    fn type_float8_is_float() {
        assert_eq!(oid_to_sqlite_type(701), SQLITE_FLOAT);
    }

    #[test]
    fn type_numeric_is_float() {
        assert_eq!(oid_to_sqlite_type(1700), SQLITE_FLOAT);
    }

    #[test]
    fn type_bytea_is_blob() {
        assert_eq!(oid_to_sqlite_type(17), SQLITE_BLOB);
    }

    #[test]
    fn type_text_is_text() {
        assert_eq!(oid_to_sqlite_type(25), SQLITE_TEXT);
    }

    #[test]
    fn type_bpchar_is_text() {
        assert_eq!(oid_to_sqlite_type(1042), SQLITE_TEXT);
    }

    #[test]
    fn type_varchar_is_text() {
        assert_eq!(oid_to_sqlite_type(1043), SQLITE_TEXT);
    }

    #[test]
    fn type_unknown_oid_is_text() {
        assert_eq!(oid_to_sqlite_type(9999), SQLITE_TEXT);
    }

    #[test]
    fn decltype_int8_is_bigint() {
        assert_eq!(oid_to_sqlite_decltype(20).to_str().unwrap(), "BIGINT");
    }

    #[test]
    fn decltype_bool_is_integer() {
        assert_eq!(oid_to_sqlite_decltype(16).to_str().unwrap(), "INTEGER");
    }

    #[test]
    fn decltype_int2_is_integer() {
        assert_eq!(oid_to_sqlite_decltype(21).to_str().unwrap(), "INTEGER");
    }

    #[test]
    fn decltype_int4_is_integer() {
        assert_eq!(oid_to_sqlite_decltype(23).to_str().unwrap(), "INTEGER");
    }

    #[test]
    fn decltype_oid_is_integer() {
        assert_eq!(oid_to_sqlite_decltype(26).to_str().unwrap(), "INTEGER");
    }

    #[test]
    fn decltype_float4_is_real() {
        assert_eq!(oid_to_sqlite_decltype(700).to_str().unwrap(), "REAL");
    }

    #[test]
    fn decltype_float8_is_real() {
        assert_eq!(oid_to_sqlite_decltype(701).to_str().unwrap(), "REAL");
    }

    #[test]
    fn decltype_numeric_is_real() {
        assert_eq!(oid_to_sqlite_decltype(1700).to_str().unwrap(), "REAL");
    }

    #[test]
    fn decltype_bytea_is_blob() {
        assert_eq!(oid_to_sqlite_decltype(17).to_str().unwrap(), "BLOB");
    }

    #[test]
    fn decltype_text_is_text() {
        assert_eq!(oid_to_sqlite_decltype(25).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_timestamp_is_text() {
        assert_eq!(oid_to_sqlite_decltype(1114).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_timestamptz_is_text() {
        assert_eq!(oid_to_sqlite_decltype(1184).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_date_is_text() {
        assert_eq!(oid_to_sqlite_decltype(1082).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_time_is_text() {
        assert_eq!(oid_to_sqlite_decltype(1083).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_unknown_oid_is_text() {
        assert_eq!(oid_to_sqlite_decltype(9999).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn upsert_non_matching_sql_returns_none() {
        assert_eq!(
            convert_metadata_settings_upsert("SELECT * FROM some_table"),
            None
        );
    }

    #[test]
    fn upsert_insert_without_table_returns_none() {
        assert_eq!(
            convert_metadata_settings_upsert("INSERT INTO other_table VALUES (1)"),
            None
        );
    }

    #[test]
    fn upsert_qualifying_insert_returns_upsert_sql() {
        let sql = "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x')";
        let result = convert_metadata_settings_upsert(sql);
        assert!(result.is_some());
        let upsert = result.unwrap();
        assert!(upsert.starts_with(sql));
        assert!(upsert.contains("ON CONFLICT (account_id, guid)"));
        assert!(upsert.contains("DO UPDATE SET"));
        assert!(upsert.contains("RETURNING id"));
    }

    #[test]
    fn upsert_already_has_on_conflict_returns_none() {
        let sql = "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x') \
                   ON CONFLICT (account_id, guid) DO NOTHING";
        assert_eq!(convert_metadata_settings_upsert(sql), None);
    }

    #[test]
    fn upsert_already_has_returning_returns_none() {
        let sql = "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x') \
                   RETURNING id";
        assert_eq!(convert_metadata_settings_upsert(sql), None);
    }

    #[test]
    fn upsert_empty_string_returns_none() {
        assert_eq!(convert_metadata_settings_upsert(""), None);
    }

    #[test]
    fn upsert_case_insensitive_match() {
        let sql = "insert into METADATA_ITEM_SETTINGS (account_id, guid) values (1, 'x')";
        let result = convert_metadata_settings_upsert(sql);
        assert!(result.is_some());
        assert!(result.unwrap().contains("ON CONFLICT"));
    }

    #[test]
    fn upsert_ffi_null_returns_null() {
        let ptr = rust_convert_metadata_settings_upsert(std::ptr::null());
        assert!(ptr.is_null());
    }

    #[test]
    fn upsert_ffi_non_matching_returns_null() {
        let input = cs("SELECT 1");
        let ptr = rust_convert_metadata_settings_upsert(input.as_ptr());
        assert!(ptr.is_null());
    }

    #[test]
    fn upsert_ffi_qualifying_returns_non_null_and_must_free() {
        let input =
            cs("INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x')");
        let ptr = rust_convert_metadata_settings_upsert(input.as_ptr());
        assert!(!ptr.is_null());
        let result = unsafe { CString::from_raw(ptr) };
        let s = result.to_str().unwrap();
        assert!(s.contains("ON CONFLICT"));
    }

    #[test]
    fn extract_url_encoded_pattern_returns_id() {
        let sql =
            "INSERT INTO play_queue_generators (uri) VALUES ('server://x%2Fmetadata%2F12345%2F')";
        assert_eq!(extract_metadata_id(sql), 12345);
    }

    #[test]
    fn extract_plain_slash_pattern_returns_id() {
        let sql =
            "INSERT INTO play_queue_generators (uri) VALUES ('server://x/metadata/67890/other')";
        assert_eq!(extract_metadata_id(sql), 67890);
    }

    #[test]
    fn extract_not_a_play_queue_insert_returns_zero() {
        let sql = "INSERT INTO some_other_table (uri) VALUES ('/metadata/999')";
        assert_eq!(extract_metadata_id(sql), 0);
    }

    #[test]
    fn extract_no_metadata_pattern_returns_zero() {
        let sql = "INSERT INTO play_queue_generators (uri) VALUES ('something-else')";
        assert_eq!(extract_metadata_id(sql), 0);
    }

    #[test]
    fn extract_empty_string_returns_zero() {
        assert_eq!(extract_metadata_id(""), 0);
    }

    #[test]
    fn extract_not_an_insert_returns_zero() {
        let sql = "SELECT * FROM play_queue_generators WHERE uri LIKE '%/metadata/1%'";
        assert_eq!(extract_metadata_id(sql), 0);
    }

    #[test]
    fn extract_single_digit_id() {
        let sql = "INSERT INTO play_queue_generators (uri) VALUES ('/metadata/7')";
        assert_eq!(extract_metadata_id(sql), 7);
    }

    #[test]
    fn extract_large_id() {
        let sql = "INSERT INTO play_queue_generators (uri) VALUES ('/metadata/9876543210')";
        assert_eq!(extract_metadata_id(sql), 9_876_543_210);
    }

    #[test]
    fn extract_ffi_null_returns_zero() {
        assert_eq!(rust_extract_metadata_id(std::ptr::null()), 0);
    }

    #[test]
    fn extract_ffi_url_encoded_returns_id() {
        let input = cs("INSERT INTO play_queue_generators (uri) VALUES ('x%2Fmetadata%2F42')");
        assert_eq!(rust_extract_metadata_id(input.as_ptr()), 42);
    }

    #[test]
    fn extract_ffi_non_matching_returns_zero() {
        let input = cs("INSERT INTO other_table VALUES (1)");
        assert_eq!(rust_extract_metadata_id(input.as_ptr()), 0);
    }

    #[test]
    fn decltype_special_case_dt_integer_for_timestamp_column() {
        let col = cs("created_at");
        let sql = cs("select created_at from t");
        let rc = rust_decltype_special_case(20, col.as_ptr(), sql.as_ptr(), 42);
        assert_eq!(rc, DECLTYPE_CASE_DT_INTEGER_8);
    }

    #[test]
    fn decltype_special_case_dt_integer_for_greatest_metadata_refresh() {
        let col = cs("greatest");
        let sql = cs("select GREATEST(max(metadata_items.changed_at), max(metadata_items.resources_changed_at))");
        let rc = rust_decltype_special_case(20, col.as_ptr(), sql.as_ptr(), 42);
        assert_eq!(rc, DECLTYPE_CASE_DT_INTEGER_8);
    }

    #[test]
    fn decltype_special_case_expression_returns_null_case() {
        let col = cs("count");
        let sql = cs("select count(*) from t");
        let rc = rust_decltype_special_case(23, col.as_ptr(), sql.as_ptr(), 0);
        assert_eq!(rc, DECLTYPE_CASE_NULL);
    }

    #[test]
    fn decltype_special_case_none_for_regular_column() {
        let col = cs("id");
        let sql = cs("select id from t");
        let rc = rust_decltype_special_case(23, col.as_ptr(), sql.as_ptr(), 123);
        assert_eq!(rc, DECLTYPE_CASE_NONE);
    }

    #[test]
    fn stmt_free_sweeps_extra_param_values_without_crash() {
        unsafe {
            let sql = cs("SELECT 1");
            let stmt = rust_stmt_create(std::ptr::null_mut(), sql.as_ptr(), std::ptr::null_mut());
            assert!(!stmt.is_null());

            (*stmt).param_count = 1;

            let a = libc::malloc(16) as *mut c_char;
            let b = libc::malloc(1024 * 1024) as *mut c_char;
            assert!(!a.is_null());
            assert!(!b.is_null());

            (*stmt).param_values[0] = a;
            (*stmt).param_values[200] = b;

            (*stmt).ref_count.store(0, Ordering::Release);
            rust_stmt_free(stmt);
        }
    }

    #[test]
    fn stmt_unref_cleans_bind_index_mismatch_slots() {
        unsafe {
            let sql = cs("SELECT ?");
            let stmt = rust_stmt_create(std::ptr::null_mut(), sql.as_ptr(), std::ptr::null_mut());
            assert!(!stmt.is_null());

            (*stmt).param_count = 1;

            for i in 1..16 {
                let buf = libc::malloc(256) as *mut c_char;
                assert!(!buf.is_null());
                *buf = b'x' as c_char;
                *buf.add(1) = 0;
                (*stmt).param_values[i] = buf;
            }

            (*stmt).ref_count.store(1, Ordering::Release);
            rust_stmt_unref(stmt);
        }
    }
}
