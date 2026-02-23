/// Module: pg_client
///
/// Pure, portable logic extracted from `pg_client.c`.
/// The libpq-dependent parts (connection lifecycle, pool management,
/// pthread logic) remain in C.  This module exposes only the tiny
/// pieces that contain no platform or library dependencies.
///
/// FFI surface (called from `src/pg_client.c`):
///   rust_hash_sql(sql)              → u64   FNV-1a hash for prepared-stmt cache keys
///   rust_is_stale_sqlstate(s)       → i32   1 if SQLSTATE == "26000"
///   rust_is_duplicate_sqlstate(s)   → i32   1 if SQLSTATE == "42P05"
use std::ffi::CStr;
use std::os::raw::c_char;

use crate::db_interpose_helpers::cstr_to_str_or_empty;

// ─── Internal pure helpers ────────────────────────────────────────────────────

/// FNV-1a hash over the bytes of `s`.
///
/// Parameters match the C implementation in `pg_client.c`:
///   - offset basis : 14695981039346656037
///   - prime        : 1099511628211
///
/// Produces identical output to the C loop for every valid UTF-8 (or raw byte)
/// sequence because FNV-1a is defined over bytes, not characters.
pub(crate) fn fnv1a_str(s: &str) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

/// Returns `true` when `sqlstate` is exactly `"26000"`
/// (invalid_sql_statement_name — prepared statement does not exist).
pub(crate) fn is_stale_sqlstate(sqlstate: &str) -> bool {
    sqlstate == "26000"
}

/// Returns `true` when `sqlstate` is exactly `"42P05"`
/// (duplicate_prepared_statement — prepared statement already exists).
pub(crate) fn is_duplicate_sqlstate(sqlstate: &str) -> bool {
    sqlstate == "42P05"
}

// ─── Public C FFI functions ───────────────────────────────────────────────────

/// FNV-1a hash for SQL strings, used as prepared-statement cache keys.
///
/// Returns 0 for a NULL pointer (matching the C implementation's `if (!sql) return 0`).
/// For a non-NULL pointer the result is identical to the C loop in `pg_hash_sql`.
///
/// # Safety
/// `sql` must be NULL or a valid, NUL-terminated C string.
#[no_mangle]
pub extern "C" fn rust_hash_sql(sql: *const c_char) -> u64 {
    if sql.is_null() {
        return 0;
    }
    let s = unsafe { cstr_to_str_or_empty(sql) };
    fnv1a_str(s)
}

/// Returns 1 if `sqlstate` is `"26000"` (prepared statement does not exist), 0 otherwise.
///
/// Intended to be called from `pg_is_stale_prepared_stmt` after the C side
/// extracts the SQLSTATE with `PQresultErrorField(res, PG_DIAG_SQLSTATE)`.
///
/// Returns 0 for a NULL pointer.
///
/// # Safety
/// `sqlstate` must be NULL or a valid, NUL-terminated C string.
#[no_mangle]
pub extern "C" fn rust_is_stale_sqlstate(sqlstate: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str_or_empty(sqlstate) };
    i32::from(is_stale_sqlstate(s))
}

/// Returns 1 if `sqlstate` is `"42P05"` (duplicate prepared statement), 0 otherwise.
///
/// Intended to be called from `pg_is_duplicate_prepared_stmt` after the C side
/// extracts the SQLSTATE with `PQresultErrorField(res, PG_DIAG_SQLSTATE)`.
///
/// Returns 0 for a NULL pointer.
///
/// # Safety
/// `sqlstate` must be NULL or a valid, NUL-terminated C string.
#[no_mangle]
pub extern "C" fn rust_is_duplicate_sqlstate(sqlstate: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str_or_empty(sqlstate) };
    i32::from(is_duplicate_sqlstate(s))
}

// ─── Pool slot state constants (matching C enum pool_slot_state_t) ────────────

pub(crate) const SLOT_FREE: u8 = 0;
pub(crate) const SLOT_RESERVED: u8 = 1;
pub(crate) const SLOT_READY: u8 = 2;
pub(crate) const SLOT_RECONNECTING: u8 = 3;
pub(crate) const SLOT_ERROR: u8 = 4;

pub(crate) const POOL_SIZE_MAX: usize = 200;
pub(crate) const POOL_SIZE_DEFAULT: usize = 50;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const STMT_CACHE_SIZE: usize = 512;

// ─── Pool Slot ───────────────────────────────────────────────────────────────

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{
    AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Mutex, OnceLock};

/// A single slot in the connection pool.
/// All fields are atomic for lock-free CAS-based state transitions.
pub(crate) struct PoolSlot {
    /// Opaque pointer to C-allocated pg_connection_t (null = no connection)
    pub conn: AtomicPtr<c_void>,
    /// Thread ID of owner (0 = unowned)
    pub owner_thread: AtomicU64,
    /// Unix timestamp of last use
    pub last_used: AtomicI64,
    /// State machine: SLOT_FREE=0, SLOT_RESERVED=1, SLOT_READY=2, etc.
    pub state: AtomicU8,
    /// Monotonically increasing generation counter (detects stale TLS refs)
    pub generation: AtomicU32,
}

impl PoolSlot {
    pub fn new() -> Self {
        Self {
            conn: AtomicPtr::new(std::ptr::null_mut()),
            owner_thread: AtomicU64::new(0),
            last_used: AtomicI64::new(0),
            state: AtomicU8::new(SLOT_FREE),
            generation: AtomicU32::new(0),
        }
    }

    /// CAS: FREE → RESERVED. Returns true on success.
    pub fn try_claim_free(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_FREE,
                SLOT_RESERVED,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// CAS: READY → RECONNECTING. Returns true on success.
    pub fn try_begin_reconnect(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_READY,
                SLOT_RECONNECTING,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// CAS: ERROR → RESERVED. Returns true on success.
    pub fn try_reclaim_error(&self) -> bool {
        self.state
            .compare_exchange(
                SLOT_ERROR,
                SLOT_RESERVED,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// CAS: READY → FREE (zombie reclaim for dead threads).
    pub fn try_reclaim_zombie(&self) -> bool {
        self.state
            .compare_exchange(SLOT_READY, SLOT_FREE, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    }

    /// Set slot to READY (after successful connection creation).
    pub fn mark_ready(&self) {
        self.state.store(SLOT_READY, Ordering::Release);
    }

    /// Set slot to ERROR (after failed connection/reconnect).
    pub fn mark_error(&self) {
        self.state.store(SLOT_ERROR, Ordering::Release);
    }

    /// Release slot back to FREE (on close or cleanup).
    pub fn release(&self) {
        self.owner_thread.store(0, Ordering::Release);
        self.state.store(SLOT_FREE, Ordering::Release);
    }
}

// ─── TLS Pool Cache ──────────────────────────────────────────────────────────

use std::cell::Cell;

/// Thread-local cache for pool slot fast path.
/// Stores slot index + generation instead of raw pointer → prevents dangling refs.
#[derive(Clone, Copy)]
pub(crate) struct TlsPoolCache {
    pub db_handle: usize,
    pub slot_index: u32,
    pub generation: u32,
}

impl TlsPoolCache {
    pub const EMPTY: Self = Self {
        db_handle: 0,
        slot_index: u32::MAX,
        generation: 0,
    };

    pub fn is_empty(&self) -> bool {
        self.slot_index == u32::MAX
    }
}

thread_local! {
    static TLS_POOL_CACHE: Cell<TlsPoolCache> = const { Cell::new(TlsPoolCache::EMPTY) };
}

/// Store a pool slot reference in TLS for the fast path.
pub(crate) fn tls_pool_cache_set(db_handle: usize, slot_index: u32, generation: u32) {
    TLS_POOL_CACHE.with(|c| {
        c.set(TlsPoolCache {
            db_handle,
            slot_index,
            generation,
        });
    });
}

/// Look up the TLS-cached pool slot for the given db handle.
/// Returns Some((slot_index, generation)) if cached, None if miss.
pub(crate) fn tls_pool_cache_get(db_handle: usize) -> Option<(u32, u32)> {
    TLS_POOL_CACHE.with(|c| {
        let cache = c.get();
        if cache.db_handle == db_handle && !cache.is_empty() {
            Some((cache.slot_index, cache.generation))
        } else {
            None
        }
    })
}

/// Invalidate the TLS pool cache.
pub(crate) fn tls_pool_cache_clear() {
    TLS_POOL_CACHE.with(|c| c.set(TlsPoolCache::EMPTY));
}

// ─── Connection Registry ─────────────────────────────────────────────────────

/// Maps sqlite3* handle (as usize) → opaque pg_connection_t* (as usize).
/// For non-pooled connections registered via pg_register_connection().
pub(crate) struct ConnectionRegistry {
    map: Mutex<HashMap<usize, usize>>,
}

impl ConnectionRegistry {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, db_handle: usize, conn_ptr: usize) {
        self.map.lock().unwrap().insert(db_handle, conn_ptr);
    }

    pub fn unregister(&self, db_handle: usize) -> Option<usize> {
        self.map.lock().unwrap().remove(&db_handle)
    }

    pub fn find(&self, db_handle: usize) -> Option<usize> {
        self.map.lock().unwrap().get(&db_handle).copied()
    }

    pub fn find_any_library(&self, is_library: impl Fn(usize) -> bool) -> Option<usize> {
        self.map
            .lock()
            .unwrap()
            .values()
            .copied()
            .find(|&conn| is_library(conn))
    }

    pub fn clear(&self) {
        self.map.lock().unwrap().clear();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }
}

// ─── Db-to-Pool Mapping ─────────────────────────────────────────────────────

/// Maps sqlite3* handle (as usize) → pool slot index.
/// Tracks which open database handles are using which pool slots.
pub(crate) struct DbToPool {
    map: Mutex<HashMap<usize, usize>>,
}

impl DbToPool {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn assign(&self, db_handle: usize, slot_index: usize) {
        self.map.lock().unwrap().insert(db_handle, slot_index);
    }

    pub fn release(&self, db_handle: usize) -> Option<usize> {
        self.map.lock().unwrap().remove(&db_handle)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn find(&self, db_handle: usize) -> Option<usize> {
        self.map.lock().unwrap().get(&db_handle).copied()
    }

    pub fn clear(&self) {
        self.map.lock().unwrap().clear();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }
}

// ─── Prepared Statement Cache ────────────────────────────────────────────────

/// Per-connection prepared statement cache entry.
#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct StmtCacheEntry {
    pub sql_hash: u64,
    pub stmt_name: String,
    pub param_count: i32,
    pub last_used: i64,
}

/// Per-connection prepared statement cache (hash table with linear probing).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct StmtCache {
    entries: Vec<Option<StmtCacheEntry>>,
    count: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
impl StmtCache {
    pub fn new() -> Self {
        Self {
            entries: (0..STMT_CACHE_SIZE).map(|_| None).collect(),
            count: 0,
        }
    }

    /// Lookup by sql_hash. Returns Some(&entry) on hit, None on miss.
    pub fn lookup(&self, sql_hash: u64) -> Option<&StmtCacheEntry> {
        if sql_hash == 0 {
            return None;
        }
        let start = (sql_hash as usize) & (STMT_CACHE_SIZE - 1);
        for i in 0..STMT_CACHE_SIZE {
            let idx = (start + i) & (STMT_CACHE_SIZE - 1);
            match &self.entries[idx] {
                Some(entry) if entry.sql_hash == sql_hash => return Some(entry),
                None => return None, // empty slot = end of probe chain
                _ => continue,       // collision, keep probing
            }
        }
        None
    }

    /// Add or update entry. Returns the evicted entry's stmt_name if eviction occurred.
    pub fn add(
        &mut self,
        sql_hash: u64,
        stmt_name: &str,
        param_count: i32,
        now: i64,
    ) -> Option<String> {
        if sql_hash == 0 {
            return None;
        }
        let start = (sql_hash as usize) & (STMT_CACHE_SIZE - 1);

        // First pass: find existing or empty slot
        for i in 0..STMT_CACHE_SIZE {
            let idx = (start + i) & (STMT_CACHE_SIZE - 1);
            match &self.entries[idx] {
                Some(entry) if entry.sql_hash == sql_hash => {
                    // Update existing
                    self.entries[idx] = Some(StmtCacheEntry {
                        sql_hash,
                        stmt_name: stmt_name.to_string(),
                        param_count,
                        last_used: now,
                    });
                    return None;
                }
                None => {
                    // Empty slot, insert here
                    self.entries[idx] = Some(StmtCacheEntry {
                        sql_hash,
                        stmt_name: stmt_name.to_string(),
                        param_count,
                        last_used: now,
                    });
                    self.count += 1;
                    return None;
                }
                _ => continue,
            }
        }

        // Table is full, evict LRU entry
        let mut lru_idx = 0;
        let mut lru_time = i64::MAX;
        for (idx, entry) in self.entries.iter().enumerate() {
            if let Some(e) = entry {
                if e.last_used < lru_time {
                    lru_time = e.last_used;
                    lru_idx = idx;
                }
            }
        }
        let evicted_name = self.entries[lru_idx].as_ref().map(|e| e.stmt_name.clone());
        self.entries[lru_idx] = Some(StmtCacheEntry {
            sql_hash,
            stmt_name: stmt_name.to_string(),
            param_count,
            last_used: now,
        });
        evicted_name
    }

    /// Clear all entries. Returns names of all evicted statements (for DEALLOCATE).
    pub fn clear(&mut self) -> Vec<String> {
        let mut evicted = Vec::new();
        for entry in &mut self.entries {
            if let Some(e) = entry.take() {
                evicted.push(e.stmt_name);
            }
        }
        self.count = 0;
        evicted
    }

    pub fn count(&self) -> usize {
        self.count
    }
}

// ─── Pool Manager ────────────────────────────────────────────────────────────

/// Central pool manager holding all connection pool state.
pub(crate) struct PoolManager {
    pub slots: Vec<PoolSlot>,
    pub configured_size: AtomicUsize,
    pub idle_timeout_secs: AtomicU32,
    pub library_db_path: Mutex<Option<String>>,
    pub last_reap_time: AtomicI64,
    pub init_pid: AtomicU32,
    pub registry: ConnectionRegistry,
    pub db_to_pool: DbToPool,
    pub global_metadata_id: AtomicI64,
    pub global_last_insert_rowid: AtomicI64,
}

impl PoolManager {
    pub fn new(pool_size: usize) -> Self {
        // Always allocate POOL_SIZE_MAX slots so auto-grow can expand
        // without reallocation. Only `configured_size` limits active use.
        let mut slots = Vec::with_capacity(POOL_SIZE_MAX);
        for _ in 0..POOL_SIZE_MAX {
            slots.push(PoolSlot::new());
        }
        Self {
            slots,
            configured_size: AtomicUsize::new(pool_size),
            idle_timeout_secs: AtomicU32::new(300),
            library_db_path: Mutex::new(None),
            last_reap_time: AtomicI64::new(0),
            init_pid: AtomicU32::new(std::process::id()),
            registry: ConnectionRegistry::new(),
            db_to_pool: DbToPool::new(),
            global_metadata_id: AtomicI64::new(0),
            global_last_insert_rowid: AtomicI64::new(0),
        }
    }

    /// Get configured pool size.
    pub fn pool_size(&self) -> usize {
        self.configured_size.load(Ordering::Relaxed)
    }

    /// Check if a connection pointer is in any pool slot.
    pub fn validate_connection(&self, conn_ptr: *const c_void) -> bool {
        let size = self.pool_size();
        for i in 0..size {
            let slot = &self.slots[i];
            if slot.state.load(Ordering::Acquire) == SLOT_READY
                && slot.conn.load(Ordering::Acquire) == conn_ptr as *mut c_void
            {
                return true;
            }
        }
        false
    }

    /// Update last_used timestamp for a connection in the pool.
    pub fn touch_connection(&self, conn_ptr: *const c_void, now: i64) {
        let size = self.pool_size();
        for i in 0..size {
            let slot = &self.slots[i];
            if slot.conn.load(Ordering::Acquire) == conn_ptr as *mut c_void {
                slot.last_used.store(now, Ordering::Release);
                return;
            }
        }
    }

    /// Reset all pool state for child process after fork.
    /// Does NOT close connections (they belong to the parent).
    pub fn reset_for_child(&self) {
        let size = self.pool_size();
        for i in 0..size {
            let slot = &self.slots[i];
            slot.conn.store(std::ptr::null_mut(), Ordering::Release);
            slot.owner_thread.store(0, Ordering::Release);
            slot.last_used.store(0, Ordering::Release);
            slot.state.store(SLOT_FREE, Ordering::Release);
            slot.generation.fetch_add(1, Ordering::SeqCst);
        }
        self.registry.clear();
        self.db_to_pool.clear();
        self.init_pid.store(std::process::id(), Ordering::Release);
        tls_pool_cache_clear();
    }

    /// Scan pool for idle connections past the timeout.
    /// Returns a vec of (slot_index, conn_ptr) pairs that should be destroyed
    /// by calling C-side PQfinish. The slot's generation is bumped and state
    /// set to FREE before returning, so no other thread can use the conn.
    pub fn reap_idle(&self, now: i64) -> Vec<(usize, *mut c_void)> {
        let timeout = self.idle_timeout_secs.load(Ordering::Relaxed) as i64;
        let size = self.pool_size();
        let mut to_destroy = Vec::new();

        for i in 0..size {
            let slot = &self.slots[i];
            let state = slot.state.load(Ordering::Acquire);

            // Only reap FREE slots that still have a connection (released but not destroyed)
            if state != SLOT_FREE {
                continue;
            }
            let conn = slot.conn.load(Ordering::Acquire);
            if conn.is_null() {
                continue;
            }
            let last_used = slot.last_used.load(Ordering::Acquire);
            if now - last_used < timeout {
                continue;
            }

            // CAS: FREE → RESERVED (claim for reaping)
            if !slot.try_claim_free() {
                continue; // another thread claimed it first
            }

            // Bump generation BEFORE taking the pointer — invalidates all TLS caches
            slot.generation.fetch_add(1, Ordering::SeqCst);

            // Extract the connection pointer
            let conn = slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);

            // Release slot back to FREE (now with null conn)
            slot.owner_thread.store(0, Ordering::Release);
            slot.state.store(SLOT_FREE, Ordering::Release);

            if !conn.is_null() {
                to_destroy.push((i, conn));
            }
        }

        to_destroy
    }
}

// ─── Global Pool Instance ────────────────────────────────────────────────────

static POOL: OnceLock<PoolManager> = OnceLock::new();

/// Get or initialize the global pool manager.
pub(crate) fn pool() -> &'static PoolManager {
    POOL.get_or_init(|| PoolManager::new(POOL_SIZE_DEFAULT))
}

// ─── Per-connection StmtCache registry (keyed by conn ptr) ───────────────────

/// Maps pg_connection_t* (as usize) → StmtCache.
/// Each pool connection has its own prepared statement cache.
static STMT_CACHES: OnceLock<Mutex<HashMap<usize, StmtCache>>> = OnceLock::new();

// ─── C Callback Types ────────────────────────────────────────────────────────
//
// These function pointers are set once at init time by C calling
// rust_pool_set_callbacks(). They allow Rust to invoke libpq-dependent
// operations that must remain in C.

/// C callback function pointer types for pool operations.
/// All callbacks receive/return opaque pointers (void*) to pg_connection_t.
#[allow(non_camel_case_types)]
type CbCreateConn = unsafe extern "C" fn(db_path: *const c_char) -> *mut c_void;
#[allow(non_camel_case_types)]
type CbDestroyConn = unsafe extern "C" fn(conn: *mut c_void);
#[allow(non_camel_case_types)]
type CbCheckConnOk = unsafe extern "C" fn(conn: *mut c_void) -> i32;
#[allow(non_camel_case_types)]
type CbResetConn = unsafe extern "C" fn(conn: *mut c_void) -> i32;
#[allow(non_camel_case_types)]
type CbReconnectSlot = unsafe extern "C" fn(conn: *mut c_void) -> i32;
#[allow(non_camel_case_types)]
type CbGetTxnStatus = unsafe extern "C" fn(conn: *mut c_void) -> i32;
#[allow(non_camel_case_types)]
type CbExecSimple = unsafe extern "C" fn(conn: *mut c_void, sql: *const c_char) -> i32;
#[allow(non_camel_case_types)]
type CbIsStreamingActive = unsafe extern "C" fn(conn: *mut c_void) -> i32;
#[allow(non_camel_case_types)]
type CbIsPgActive = unsafe extern "C" fn(conn: *mut c_void) -> i32;
#[allow(non_camel_case_types)]
type CbSetPgActive = unsafe extern "C" fn(conn: *mut c_void, active: i32);
#[allow(non_camel_case_types)]
type CbCheckThreadAlive = unsafe extern "C" fn(thread_id: u64) -> i32;
#[allow(non_camel_case_types)]
type CbStmtCacheClear = unsafe extern "C" fn(conn: *mut c_void);
#[allow(non_camel_case_types)]
type CbGetDbPath = unsafe extern "C" fn(conn: *mut c_void, buf: *mut c_char, len: usize);
#[allow(non_camel_case_types)]
type CbGetCurrentThread = unsafe extern "C" fn() -> u64;
#[allow(non_camel_case_types)]
type CbThreadsEqual = unsafe extern "C" fn(a: u64, b: u64) -> i32;
#[allow(non_camel_case_types)]
type CbSleepMs = unsafe extern "C" fn(ms: i32);
#[allow(non_camel_case_types)]
type CbGetRetryDelays = unsafe extern "C" fn(delays: *mut i32, count: *mut i32);
#[allow(non_camel_case_types)]
type CbLogInfo = unsafe extern "C" fn(msg: *const c_char);
#[allow(non_camel_case_types)]
type CbLogError = unsafe extern "C" fn(msg: *const c_char);
#[allow(non_camel_case_types)]
type CbLogDebug = unsafe extern "C" fn(msg: *const c_char);

/// Storage for all C callback function pointers.
struct PoolCallbacks {
    create_conn: CbCreateConn,
    destroy_conn: CbDestroyConn,
    check_conn_ok: CbCheckConnOk,
    reset_conn: CbResetConn,
    reconnect_slot: CbReconnectSlot,
    get_txn_status: CbGetTxnStatus,
    exec_simple: CbExecSimple,
    is_streaming_active: CbIsStreamingActive,
    is_pg_active: CbIsPgActive,
    #[allow(dead_code)]
    set_pg_active: CbSetPgActive,
    check_thread_alive: CbCheckThreadAlive,
    stmt_cache_clear: CbStmtCacheClear,
    get_db_path: CbGetDbPath,
    get_current_thread: CbGetCurrentThread,
    threads_equal: CbThreadsEqual,
    sleep_ms: CbSleepMs,
    get_retry_delays: CbGetRetryDelays,
    log_info: CbLogInfo,
    log_error: CbLogError,
    log_debug: CbLogDebug,
}

// SAFETY: PoolCallbacks contains only function pointers which are Send+Sync
unsafe impl Send for PoolCallbacks {}
unsafe impl Sync for PoolCallbacks {}

static CALLBACKS: OnceLock<PoolCallbacks> = OnceLock::new();

fn cb() -> &'static PoolCallbacks {
    CALLBACKS.get().expect("pool callbacks not registered — call rust_pool_set_callbacks first")
}

// ─── Logging helpers ─────────────────────────────────────────────────────────

fn log_info(msg: &str) {
    if let Some(cbs) = CALLBACKS.get() {
        if let Ok(cs) = std::ffi::CString::new(msg) {
            unsafe { (cbs.log_info)(cs.as_ptr()); }
        }
    }
}

fn log_error(msg: &str) {
    if let Some(cbs) = CALLBACKS.get() {
        if let Ok(cs) = std::ffi::CString::new(msg) {
            unsafe { (cbs.log_error)(cs.as_ptr()); }
        }
    }
}

fn log_debug(msg: &str) {
    if let Some(cbs) = CALLBACKS.get() {
        if let Ok(cs) = std::ffi::CString::new(msg) {
            unsafe { (cbs.log_debug)(cs.as_ptr()); }
        }
    }
}

// ─── Pool algorithm helpers ──────────────────────────────────────────────────

/// Transaction status constants (matching PGTransactionStatusType)
const PQTRANS_INTRANS: i32 = 2;
const PQTRANS_INERROR: i32 = 3;

/// Check if a db_path is for library.db (suffix match).
fn is_library_db(path: &str) -> bool {
    path.ends_with("com.plexapp.plugins.library.db")
}

// Thread-local retry counter for pool_get_connection recursive retry.
thread_local! {
    static POOL_RETRY_COUNT: Cell<i32> = const { Cell::new(0) };
}

/// Maximum retry delays from config.
const MAX_RETRY_DELAYS: usize = 10;

// ─── Pool Get Connection: 7-Phase Algorithm ──────────────────────────────────
//
// This is the core pool algorithm, migrated from C's pool_get_connection().
// All libpq operations are done through C callbacks. All state management
// (slot claiming, TLS cache, generation checks) is in Rust.

/// Internal: full 7-phase pool acquisition.
/// Returns an opaque pg_connection_t* or null.
fn pool_get_connection_inner(db_path: *const c_char) -> *mut c_void {
    let cbs = cb();
    let pm = pool();

    // Convert db_path to &str for is_library_db check
    let path_str = if db_path.is_null() {
        ""
    } else {
        unsafe { cstr_to_str_or_empty(db_path) }
    };

    if !is_library_db(path_str) {
        return std::ptr::null_mut();
    }

    let current_thread = unsafe { (cbs.get_current_thread)() };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Save library_db_path (first time only)
    {
        let mut lib_path = pm.library_db_path.lock().unwrap();
        if lib_path.is_none() && !path_str.is_empty() {
            *lib_path = Some(path_str.to_string());
        }
    }

    let pool_size = pm.pool_size();

    // =========================================================================
    // FAST PATH: Check TLS-cached slot (O(1))
    // =========================================================================
    if let Some((idx, gen)) = tls_pool_cache_get(0) {
        let idx = idx as usize;
        if idx < pool_size {
            let slot = &pm.slots[idx];
            if slot.state.load(Ordering::Acquire) == SLOT_READY
                && slot.generation.load(Ordering::Acquire) == gen
            {
                let owner = slot.owner_thread.load(Ordering::Acquire);
                if unsafe { (cbs.threads_equal)(owner, current_thread) } != 0 {
                    let conn = slot.conn.load(Ordering::Acquire);
                    if !conn.is_null()
                        && unsafe { (cbs.check_conn_ok)(conn) } != 0
                    {
                        // Skip if streaming
                        if unsafe { (cbs.is_streaming_active)(conn) } != 0 {
                            log_debug(&format!(
                                "Pool FAST PATH: streaming_active on slot {}, falling through",
                                idx
                            ));
                        } else {
                            slot.last_used.store(now, Ordering::Release);
                            return conn;
                        }
                    }
                }
            }
        }
        // Cached slot invalid — clear and fall through
        tls_pool_cache_clear();
    }

    // =========================================================================
    // PHASE 0: Cleanup zombie READY connections from dead threads
    // =========================================================================
    let idle_timeout = pm.idle_timeout_secs.load(Ordering::Relaxed) as i64;

    for i in 0..pool_size {
        let slot = &pm.slots[i];
        let state = slot.state.load(Ordering::Acquire);
        if state != SLOT_READY {
            continue;
        }
        let last_used = slot.last_used.load(Ordering::Acquire);
        if now - last_used <= idle_timeout {
            continue;
        }

        let owner = slot.owner_thread.load(Ordering::Acquire);
        if unsafe { (cbs.check_thread_alive)(owner) } != 0 {
            continue; // Thread alive — don't touch
        }

        // Thread is dead → safe to reclaim, unless streaming
        let conn = slot.conn.load(Ordering::Acquire);
        if !conn.is_null() && unsafe { (cbs.is_streaming_active)(conn) } != 0 {
            log_info(&format!(
                "Pool PHASE 0: slot {} owner dead but streaming_active, skipping reclaim",
                i
            ));
            continue;
        }

        // CAS: READY → FREE
        if slot.try_reclaim_zombie() {
            log_info(&format!(
                "Pool PHASE 0: Freed zombie slot {} (owner thread dead, idle {} sec)",
                i,
                now - last_used
            ));
        }
    }

    // Run pool reaper periodically
    let last_reap = pm.last_reap_time.load(Ordering::Relaxed);
    if now - last_reap >= 60 {
        // CAS to avoid multiple threads running reaper simultaneously
        if pm
            .last_reap_time
            .compare_exchange(last_reap, now, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            log_info(&format!(
                "Pool reaper: running (last run {} seconds ago)",
                now - last_reap
            ));
            let to_destroy = pm.reap_idle(now);
            for (_slot_idx, conn_ptr) in to_destroy {
                unsafe { (cbs.destroy_conn)(conn_ptr); }
            }
        }
    }

    // =========================================================================
    // PHASE 1: Find thread's existing READY connection (lock-free)
    // =========================================================================
    for i in 0..pool_size {
        let slot = &pm.slots[i];
        let state = slot.state.load(Ordering::Acquire);
        if state != SLOT_READY {
            continue;
        }
        let owner = slot.owner_thread.load(Ordering::Acquire);
        if unsafe { (cbs.threads_equal)(owner, current_thread) } == 0 {
            continue;
        }

        let conn = slot.conn.load(Ordering::Acquire);
        if !conn.is_null() && unsafe { (cbs.check_conn_ok)(conn) } != 0 {
            // Skip streaming connections
            if unsafe { (cbs.is_streaming_active)(conn) } != 0 {
                log_debug(&format!(
                    "Pool: slot {} streaming_active, skipping for thread",
                    i
                ));
                continue;
            }
            slot.last_used.store(now, Ordering::Release);
            tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
            return conn;
        }

        // Connection is dead — try READY → RECONNECTING
        if slot.try_begin_reconnect() {
            unsafe { (cbs.stmt_cache_clear)(conn); }
            let ok = unsafe { (cbs.reconnect_slot)(conn) };
            if ok != 0 {
                slot.last_used.store(now, Ordering::Release);
                slot.mark_ready();
                tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
                return conn;
            } else {
                slot.mark_error();
                return std::ptr::null_mut();
            }
        }
    }

    // =========================================================================
    // PHASE 2: Claim FREE slot with existing connection (reuse released slots)
    // =========================================================================
    for i in 0..pool_size {
        let slot = &pm.slots[i];
        let conn = slot.conn.load(Ordering::Acquire);
        if conn.is_null() {
            continue;
        }

        // Skip streaming connections
        if unsafe { (cbs.is_streaming_active)(conn) } != 0 {
            continue;
        }

        if !slot.try_claim_free() {
            continue;
        }

        // Successfully claimed slot
        slot.owner_thread.store(current_thread, Ordering::Release);
        slot.last_used.store(now, Ordering::Release);
        slot.generation.fetch_add(1, Ordering::SeqCst);

        // Commit/rollback any pending transaction before reset
        let txn = unsafe { (cbs.get_txn_status)(conn) };
        if txn == PQTRANS_INTRANS || txn == PQTRANS_INERROR {
            let cmd = if txn == PQTRANS_INTRANS {
                c"COMMIT"
            } else {
                c"ROLLBACK"
            };
            log_info(&format!(
                "Pool PHASE 2: slot {} has pending transaction (status={}), sending cleanup before reset",
                i, txn
            ));
            unsafe { (cbs.exec_simple)(conn, cmd.as_ptr()); }
        }

        // Clear stmt cache and reset connection
        unsafe { (cbs.stmt_cache_clear)(conn); }
        let reset_ok = unsafe { (cbs.reset_conn)(conn) };

        if reset_ok != 0 {
            log_debug(&format!("Pool: reusing reset connection in slot {}", i));
            slot.mark_ready();
            tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
            return conn;
        }

        // Reset failed — do full reconnect
        unsafe { (cbs.stmt_cache_clear)(conn); }
        let reconn_ok = unsafe { (cbs.reconnect_slot)(conn) };
        if reconn_ok != 0 {
            slot.last_used.store(now, Ordering::Release);
            slot.mark_ready();
            tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
            return conn;
        } else {
            slot.mark_error();
            // Continue trying other slots
        }
    }

    // =========================================================================
    // PHASE 3: Find empty FREE slot and create new connection
    // =========================================================================
    for i in 0..pool_size {
        let slot = &pm.slots[i];
        if !slot.conn.load(Ordering::Acquire).is_null() {
            continue; // Only try empty slots
        }

        if !slot.try_claim_free() {
            continue;
        }

        slot.owner_thread.store(current_thread, Ordering::Release);
        slot.last_used.store(now, Ordering::Release);
        slot.generation.fetch_add(1, Ordering::SeqCst);

        log_debug(&format!("Pool: claimed empty slot {} for thread", i));

        let new_conn = unsafe { (cbs.create_conn)(db_path) };
        if !new_conn.is_null() && unsafe { (cbs.is_pg_active)(new_conn) } != 0 {
            slot.conn.store(new_conn, Ordering::Release);
            log_info(&format!("Pool: created new connection in slot {}", i));
            slot.mark_ready();
            tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
            return new_conn;
        } else {
            // Creation failed — release slot
            log_error(&format!("Pool: failed to create connection for slot {}", i));
            if !new_conn.is_null() {
                unsafe { (cbs.destroy_conn)(new_conn); }
            }
            slot.conn.store(std::ptr::null_mut(), Ordering::Release);
            slot.owner_thread.store(0, Ordering::Release);
            slot.release();
            // Continue trying other slots
        }
    }

    // =========================================================================
    // PHASE 4: Try to claim ERROR slots (failed connections that need retry)
    // =========================================================================
    for i in 0..pool_size {
        let slot = &pm.slots[i];
        if !slot.try_reclaim_error() {
            continue;
        }

        slot.owner_thread.store(current_thread, Ordering::Release);
        slot.last_used.store(now, Ordering::Release);
        slot.generation.fetch_add(1, Ordering::SeqCst);

        // Free old connection if any
        let old_conn = slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !old_conn.is_null() {
            unsafe { (cbs.destroy_conn)(old_conn); }
        }

        log_debug(&format!("Pool: reclaiming error slot {}", i));

        let new_conn = unsafe { (cbs.create_conn)(db_path) };
        if !new_conn.is_null() && unsafe { (cbs.is_pg_active)(new_conn) } != 0 {
            slot.conn.store(new_conn, Ordering::Release);
            log_info(&format!("Pool: recovered slot {} with new connection", i));
            slot.mark_ready();
            tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
            return new_conn;
        } else {
            if !new_conn.is_null() {
                unsafe { (cbs.destroy_conn)(new_conn); }
            }
            slot.conn.store(std::ptr::null_mut(), Ordering::Release);
            slot.owner_thread.store(0, Ordering::Release);
            slot.release();
        }
    }

    // =========================================================================
    // PHASE 5: Auto-grow pool
    // =========================================================================
    let current_size = pm.configured_size.load(Ordering::Relaxed);
    if current_size < POOL_SIZE_MAX {
        let new_size = current_size + 1;
        if pm
            .configured_size
            .compare_exchange(current_size, new_size, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            let idx = new_size - 1;
            if idx < pm.slots.len() {
                let slot = &pm.slots[idx];
                if slot.try_claim_free() {
                    slot.owner_thread.store(current_thread, Ordering::Release);
                    slot.last_used.store(now, Ordering::Release);
                    slot.generation.fetch_add(1, Ordering::SeqCst);

                    log_error(&format!(
                        "Pool: auto-grew {} -> {} (thread needs slot)",
                        current_size, new_size
                    ));

                    let new_conn = unsafe { (cbs.create_conn)(db_path) };
                    if !new_conn.is_null()
                        && unsafe { (cbs.is_pg_active)(new_conn) } != 0
                    {
                        slot.conn.store(new_conn, Ordering::Release);
                        slot.mark_ready();
                        tls_pool_cache_set(
                            0,
                            idx as u32,
                            slot.generation.load(Ordering::Acquire),
                        );
                        return new_conn;
                    } else {
                        log_error(&format!("Pool: auto-grow slot {} connection failed", idx));
                        if !new_conn.is_null() {
                            unsafe { (cbs.destroy_conn)(new_conn); }
                        }
                        slot.conn.store(std::ptr::null_mut(), Ordering::Release);
                        slot.owner_thread.store(0, Ordering::Release);
                        slot.release();
                    }
                }
            }
        }
    }

    // =========================================================================
    // PHASE 6: Retry with backoff
    // =========================================================================
    let retry_count = POOL_RETRY_COUNT.with(|c| c.get());

    let mut delays = [0i32; MAX_RETRY_DELAYS];
    let mut max_retries = 0i32;
    unsafe { (cbs.get_retry_delays)(delays.as_mut_ptr(), &mut max_retries); }

    if retry_count < max_retries {
        let delay = delays[retry_count as usize];
        log_error(&format!(
            "Pool: no connection available, retry {}/{} in {}ms",
            retry_count + 1,
            max_retries,
            delay
        ));
        POOL_RETRY_COUNT.with(|c| c.set(retry_count + 1));
        unsafe { (cbs.sleep_ms)(delay); }

        // Recursive retry
        let result = pool_get_connection_inner(db_path);
        if !result.is_null() {
            POOL_RETRY_COUNT.with(|c| c.set(0));
        }
        return result;
    }

    // All retries exhausted
    log_error(&format!(
        "Pool: no available slots after {} retries (all {} slots busy)",
        max_retries,
        pm.configured_size.load(Ordering::Relaxed)
    ));
    POOL_RETRY_COUNT.with(|c| c.set(0));
    std::ptr::null_mut()
}

// ─── Pool Release (close_for_db) ────────────────────────────────────────────

/// Release a pool slot when a database handle is closed.
/// The connection stays open in the pool for potential reuse.
fn pool_release_for_db_inner(db_handle: usize) {
    let pm = pool();
    let cbs = cb();

    // Remove db_to_pool mapping
    let slot_opt = pm.db_to_pool.release(db_handle);

    if let Some(slot_idx) = slot_opt {
        let pool_size = pm.pool_size();
        if slot_idx < pool_size {
            let slot = &pm.slots[slot_idx];
            let current_thread = unsafe { (cbs.get_current_thread)() };
            let owner = slot.owner_thread.load(Ordering::Acquire);

            if unsafe { (cbs.threads_equal)(owner, current_thread) } != 0 {
                let state = slot.state.load(Ordering::Acquire);
                if state == SLOT_READY {
                    // Commit/rollback pending transaction before release
                    let conn = slot.conn.load(Ordering::Acquire);
                    if !conn.is_null() {
                        let txn = unsafe { (cbs.get_txn_status)(conn) };
                        if txn == PQTRANS_INTRANS || txn == PQTRANS_INERROR {
                            let cmd = if txn == PQTRANS_INTRANS {
                                c"COMMIT"
                            } else {
                                c"ROLLBACK"
                            };
                            log_info(&format!(
                                "Pool: slot {} has pending transaction (status={}), sending cleanup before release",
                                slot_idx, txn
                            ));
                            unsafe { (cbs.exec_simple)(conn, cmd.as_ptr()); }
                        }
                    }

                    slot.owner_thread.store(0, Ordering::Release);
                    slot.state.store(SLOT_FREE, Ordering::Release);
                    log_info(&format!(
                        "Pool: releasing slot {} for db {:x}",
                        slot_idx, db_handle
                    ));
                }
            }
        }
    }

    // Clear TLS cache
    tls_pool_cache_clear();
}

// ─── Pool Health Check ───────────────────────────────────────────────────────

/// Check connection health after query error, reset if corrupted.
/// Returns 1 if connection was reset, 0 if still healthy.
fn pool_check_health_inner(conn: *mut c_void) -> i32 {
    if conn.is_null() {
        return 0;
    }

    let cbs = cb();

    // Check if connection is still OK
    if unsafe { (cbs.check_conn_ok)(conn) } != 0 {
        return 0; // Healthy
    }

    log_info("Pool: connection health check failed, resetting");

    let pm = pool();
    let current_thread = unsafe { (cbs.get_current_thread)() };
    let pool_size = pm.pool_size();

    for i in 0..pool_size {
        let slot = &pm.slots[i];
        if slot.conn.load(Ordering::Acquire) != conn {
            continue;
        }
        let owner = slot.owner_thread.load(Ordering::Acquire);
        if unsafe { (cbs.threads_equal)(owner, current_thread) } == 0 {
            continue;
        }

        // Try READY → RECONNECTING
        if !slot.try_begin_reconnect() {
            break;
        }

        unsafe { (cbs.stmt_cache_clear)(conn); }

        // Try PQreset first
        let reset_ok = unsafe { (cbs.reset_conn)(conn) };
        if reset_ok != 0 {
            log_info(&format!("Pool: connection reset successful for slot {}", i));
            slot.last_used.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                Ordering::Release,
            );
            slot.mark_ready();
            return 1;
        }

        // PQreset failed — try full reconnect
        log_error(&format!(
            "Pool: PQreset failed for slot {}, trying fresh connection...",
            i
        ));
        let reconn_ok = unsafe { (cbs.reconnect_slot)(conn) };
        if reconn_ok != 0 {
            slot.last_used.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                Ordering::Release,
            );
            log_info(&format!(
                "Pool: fresh connection succeeded for slot {} (reconnected)",
                i
            ));
            slot.mark_ready();
            return 1;
        } else {
            log_error(&format!(
                "Pool: fresh connection also failed for slot {}",
                i
            ));
            slot.mark_error();
            return 1;
        }
    }

    0
}

// ─── Find Connection (for pg_find_connection) ────────────────────────────────

/// Find the pool connection for a given database handle.
/// This is the Rust equivalent of the C pg_find_connection logic for pooled conns.
/// Returns the pool connection pointer, or null if not a library.db handle.
fn pool_find_connection_for_db(db_handle: usize, db_path: *const c_char) -> *mut c_void {
    let cbs = cb();
    let pm = pool();

    let path_str = if db_path.is_null() {
        ""
    } else {
        unsafe { cstr_to_str_or_empty(db_path) }
    };

    if !is_library_db(path_str) {
        return std::ptr::null_mut();
    }

    // Get pool connection
    let pool_conn = pool_get_connection_inner(db_path);
    if pool_conn.is_null() {
        return std::ptr::null_mut();
    }

    if unsafe { (cbs.is_pg_active)(pool_conn) } == 0 {
        return std::ptr::null_mut();
    }

    // Track db→pool mapping
    let pool_size = pm.pool_size();
    for i in 0..pool_size {
        let slot = &pm.slots[i];
        if slot.conn.load(Ordering::Acquire) == pool_conn {
            pm.db_to_pool.assign(db_handle, i);
            log_debug(&format!("Tracked db {:x} -> pool slot {}", db_handle, i));
            break;
        }
    }

    // Update TLS cache with db_handle for the fast path
    // (Re-read the TLS cache to get the current slot info)
    if let Some((idx, gen)) = tls_pool_cache_get(0) {
        tls_pool_cache_set(db_handle, idx, gen);
    }

    pool_conn
}

// ═════════════════════════════════════════════════════════════════════════════
// Public C FFI — Pool Operations
// ═════════════════════════════════════════════════════════════════════════════

/// Register all C callback function pointers with the Rust pool module.
/// Must be called once at init time before any pool operations.
///
/// # Safety
/// All function pointers must be valid, non-null C function pointers.
#[no_mangle]
pub unsafe extern "C" fn rust_pool_set_callbacks(
    create_conn: CbCreateConn,
    destroy_conn: CbDestroyConn,
    check_conn_ok: CbCheckConnOk,
    reset_conn: CbResetConn,
    reconnect_slot: CbReconnectSlot,
    get_txn_status: CbGetTxnStatus,
    exec_simple: CbExecSimple,
    is_streaming_active: CbIsStreamingActive,
    is_pg_active: CbIsPgActive,
    set_pg_active: CbSetPgActive,
    check_thread_alive: CbCheckThreadAlive,
    stmt_cache_clear: CbStmtCacheClear,
    get_db_path: CbGetDbPath,
    get_current_thread: CbGetCurrentThread,
    threads_equal: CbThreadsEqual,
    sleep_ms: CbSleepMs,
    get_retry_delays: CbGetRetryDelays,
    log_info_cb: CbLogInfo,
    log_error_cb: CbLogError,
    log_debug_cb: CbLogDebug,
) {
    let _ = CALLBACKS.set(PoolCallbacks {
        create_conn,
        destroy_conn,
        check_conn_ok,
        reset_conn,
        reconnect_slot,
        get_txn_status,
        exec_simple,
        is_streaming_active,
        is_pg_active,
        set_pg_active,
        check_thread_alive,
        stmt_cache_clear,
        get_db_path,
        get_current_thread,
        threads_equal,
        sleep_ms,
        get_retry_delays,
        log_info: log_info_cb,
        log_error: log_error_cb,
        log_debug: log_debug_cb,
    });
}

/// Initialize the pool with optional pool_size and idle_timeout from env vars.
/// Called from pg_client_init().
///
/// # Safety
/// Must be called after rust_pool_set_callbacks().
#[no_mangle]
pub extern "C" fn rust_pool_init(pool_size: i32, idle_timeout: i32) {
    let pm = pool(); // Initialize via OnceLock
    if pool_size > 0 && pool_size <= POOL_SIZE_MAX as i32 {
        pm.configured_size
            .store(pool_size as usize, Ordering::Relaxed);
    }
    if idle_timeout >= 10 {
        pm.idle_timeout_secs
            .store(idle_timeout as u32, Ordering::Relaxed);
    }
    pm.init_pid.store(std::process::id(), Ordering::Release);
}

/// Clean up all pool resources. Called from pg_client_cleanup().
///
/// # Safety
/// Must not be called concurrently.
#[no_mangle]
pub extern "C" fn rust_pool_cleanup() {
    if CALLBACKS.get().is_none() {
        return;
    }
    let cbs = cb();
    let pm = pool();
    let pool_size = pm.pool_size();

    for i in 0..pool_size {
        let slot = &pm.slots[i];
        // Force to FREE
        slot.state.store(SLOT_FREE, Ordering::SeqCst);

        let conn = slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !conn.is_null() {
            unsafe { (cbs.destroy_conn)(conn); }
        }
        slot.owner_thread.store(0, Ordering::Release);
        slot.generation.store(0, Ordering::Release);
    }

    pm.db_to_pool.clear();
    pm.registry.clear();

    // Clear stmt caches
    if let Some(caches) = STMT_CACHES.get() {
        caches.lock().unwrap().clear();
    }
}

/// Get a pool connection for the given db_path.
/// This is the main entry point — replaces the C pool_get_connection().
///
/// # Safety
/// `db_path` must be NULL or a valid C string.
#[no_mangle]
pub unsafe extern "C" fn rust_pool_get_connection(db_path: *const c_char) -> *mut c_void {
    pool_get_connection_inner(db_path)
}

/// Release pool slot for a database handle (called on sqlite3_close).
///
/// # Safety
/// `db` must be a valid sqlite3* pointer (cast to void*).
#[no_mangle]
pub extern "C" fn rust_pool_release_for_db(db: *const c_void) {
    pool_release_for_db_inner(db as usize);
}

/// Validate that a connection pointer is still in the pool.
/// Returns 1 if valid, 0 if not found.
///
/// # Safety
/// `conn` may be any pointer value (validation is the point).
#[no_mangle]
pub extern "C" fn rust_pool_validate_connection(conn: *const c_void) -> i32 {
    i32::from(pool().validate_connection(conn))
}

/// Update last_used timestamp for a pool connection.
///
/// # Safety
/// `conn` must be a valid pg_connection_t pointer in the pool.
#[no_mangle]
pub extern "C" fn rust_pool_touch_connection(conn: *const c_void) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    pool().touch_connection(conn, now);
}

/// Check connection health after query error.
/// Returns 1 if connection was reset, 0 if still healthy.
///
/// # Safety
/// `conn` must be a valid pg_connection_t pointer.
#[no_mangle]
pub extern "C" fn rust_pool_check_health(conn: *mut c_void) -> i32 {
    pool_check_health_inner(conn)
}

/// Reset pool state for child process after fork.
///
/// # Safety
/// Must be called from the child process only.
#[no_mangle]
pub extern "C" fn rust_pool_cleanup_after_fork() {
    pool().reset_for_child();
}

/// Register a non-pooled connection in the registry.
///
/// # Safety
/// Both pointers must be valid.
#[no_mangle]
pub extern "C" fn rust_register_connection(db_handle: *const c_void, conn: *const c_void) {
    pool().registry.register(db_handle as usize, conn as usize);
}

/// Unregister a non-pooled connection from the registry.
///
/// # Safety
/// `db_handle` must be a valid sqlite3* pointer.
#[no_mangle]
pub extern "C" fn rust_unregister_connection(db_handle: *const c_void) {
    pool().registry.unregister(db_handle as usize);
}

/// Find a registered (non-pooled) connection for a db handle.
///
/// # Safety
/// `db_handle` must be a valid sqlite3* pointer.
#[no_mangle]
pub extern "C" fn rust_find_registered_connection(db_handle: *const c_void) -> *mut c_void {
    pool()
        .registry
        .find(db_handle as usize)
        .map(|p| p as *mut c_void)
        .unwrap_or(std::ptr::null_mut())
}

/// Find the pool connection for a database handle, getting one from the pool
/// if necessary. Used by pg_find_connection().
///
/// # Safety
/// `db_handle` must be a valid sqlite3* pointer, `db_path` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn rust_pool_find_connection(
    db_handle: *const c_void,
    db_path: *const c_char,
) -> *mut c_void {
    pool_find_connection_for_db(db_handle as usize, db_path)
}

/// Find any library connection from the registry.
///
/// # Safety
/// The returned pointer may be null.
#[no_mangle]
pub extern "C" fn rust_find_any_library_connection() -> *mut c_void {
    if CALLBACKS.get().is_none() {
        return std::ptr::null_mut();
    }
    let cbs = cb();
    let pm = pool();

    // First try pool
    let lib_path = pm.library_db_path.lock().unwrap().clone();
    if let Some(path) = lib_path {
        if let Ok(cs) = std::ffi::CString::new(path) {
            let conn = pool_get_connection_inner(cs.as_ptr());
            if !conn.is_null() && unsafe { (cbs.is_pg_active)(conn) } != 0 {
                return conn;
            }
        }
    }

    // Fall back to registry: find any library connection
    pm.registry
        .find_any_library(|conn_ptr| {
            let conn = conn_ptr as *mut c_void;
            if unsafe { (cbs.is_pg_active)(conn) } == 0 {
                return false;
            }
            let mut buf = [0u8; 1024];
            unsafe {
                (cbs.get_db_path)(conn, buf.as_mut_ptr() as *mut c_char, buf.len());
            }
            let path = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
                .to_str()
                .unwrap_or("");
            is_library_db(path)
        })
        .map(|p| p as *mut c_void)
        .unwrap_or(std::ptr::null_mut())
}

/// Get global metadata ID (atomic).
#[no_mangle]
pub extern "C" fn rust_get_global_metadata_id() -> i64 {
    pool().global_metadata_id.load(Ordering::SeqCst)
}

/// Set global metadata ID (atomic).
#[no_mangle]
pub extern "C" fn rust_set_global_metadata_id(id: i64) {
    pool().global_metadata_id.store(id, Ordering::SeqCst);
}

/// Get global last_insert_rowid (atomic).
#[no_mangle]
pub extern "C" fn rust_get_global_last_insert_rowid() -> i64 {
    pool().global_last_insert_rowid.load(Ordering::SeqCst)
}

/// Set global last_insert_rowid (atomic).
#[no_mangle]
pub extern "C" fn rust_set_global_last_insert_rowid(id: i64) {
    pool().global_last_insert_rowid.store(id, Ordering::SeqCst)
}

/// Check if we're in a forked child and need to reset.
/// Returns 1 if pool was reset, 0 if same process.
#[no_mangle]
pub extern "C" fn rust_pool_check_fork() -> i32 {
    let pm = pool();
    let init_pid = pm.init_pid.load(Ordering::Acquire);
    let current_pid = std::process::id();
    if init_pid != 0 && init_pid != current_pid {
        pm.reset_for_child();
        return 1;
    }
    0
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// Helper: create a CString from a &str (panics on interior NUL).
    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    // ── fnv1a_str / rust_hash_sql (existing tests) ──────────────────────────

    #[test]
    fn hash_null_returns_zero() {
        assert_eq!(rust_hash_sql(std::ptr::null()), 0);
    }

    #[test]
    fn hash_same_string_is_deterministic() {
        let sql = "SELECT id FROM metadata WHERE guid = $1";
        let h1 = fnv1a_str(sql);
        let h2 = fnv1a_str(sql);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_different_strings_differ() {
        let h1 = fnv1a_str("SELECT 1");
        let h2 = fnv1a_str("SELECT 2");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_empty_string_is_nonzero() {
        let h = fnv1a_str("");
        assert_ne!(h, 0);
    }

    #[test]
    fn hash_empty_string_consistent() {
        assert_eq!(fnv1a_str(""), fnv1a_str(""));
    }

    #[test]
    fn hash_known_value_matches_c_implementation() {
        let expected: u64 = {
            let mut h: u64 = 14695981039346656037;
            for b in b"SELECT 1" {
                h ^= *b as u64;
                h = h.wrapping_mul(1099511628211);
            }
            h
        };
        assert_eq!(fnv1a_str("SELECT 1"), expected);
    }

    #[test]
    fn hash_similar_strings_differ() {
        let h1 = fnv1a_str("INSERT INTO t VALUES ($1)");
        let h2 = fnv1a_str("INSERT INTO t VALUES ($2)");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_ffi_nonempty_nonzero() {
        let cs = c("SELECT * FROM metadata");
        assert_ne!(rust_hash_sql(cs.as_ptr()), 0);
    }

    #[test]
    fn hash_ffi_matches_pure_helper() {
        let sql = "UPDATE metadata SET title=$1 WHERE id=$2";
        let cs = c(sql);
        assert_eq!(rust_hash_sql(cs.as_ptr()), fnv1a_str(sql));
    }

    #[test]
    fn hash_case_sensitive() {
        assert_ne!(fnv1a_str("select 1"), fnv1a_str("SELECT 1"));
    }

    // ── SQLSTATE tests (existing) ───────────────────────────────────────────

    #[test]
    fn stale_exact_match_returns_one() {
        assert_eq!(rust_is_stale_sqlstate(c("26000").as_ptr()), 1);
    }

    #[test]
    fn stale_null_returns_zero() {
        assert_eq!(rust_is_stale_sqlstate(std::ptr::null()), 0);
    }

    #[test]
    fn stale_empty_string_returns_zero() {
        assert_eq!(rust_is_stale_sqlstate(c("").as_ptr()), 0);
    }

    #[test]
    fn stale_wrong_code_42p05_returns_zero() {
        assert_eq!(rust_is_stale_sqlstate(c("42P05").as_ptr()), 0);
    }

    #[test]
    fn stale_close_but_wrong_26001_returns_zero() {
        assert_eq!(rust_is_stale_sqlstate(c("26001").as_ptr()), 0);
    }

    #[test]
    fn stale_pure_helper_true() {
        assert!(is_stale_sqlstate("26000"));
    }

    #[test]
    fn stale_pure_helper_false_for_prefix() {
        assert!(!is_stale_sqlstate("2600"));
    }

    #[test]
    fn duplicate_exact_match_returns_one() {
        assert_eq!(rust_is_duplicate_sqlstate(c("42P05").as_ptr()), 1);
    }

    #[test]
    fn duplicate_null_returns_zero() {
        assert_eq!(rust_is_duplicate_sqlstate(std::ptr::null()), 0);
    }

    #[test]
    fn duplicate_empty_string_returns_zero() {
        assert_eq!(rust_is_duplicate_sqlstate(c("").as_ptr()), 0);
    }

    #[test]
    fn duplicate_wrong_code_26000_returns_zero() {
        assert_eq!(rust_is_duplicate_sqlstate(c("26000").as_ptr()), 0);
    }

    #[test]
    fn duplicate_close_but_wrong_42p06_returns_zero() {
        assert_eq!(rust_is_duplicate_sqlstate(c("42P06").as_ptr()), 0);
    }

    #[test]
    fn duplicate_pure_helper_true() {
        assert!(is_duplicate_sqlstate("42P05"));
    }

    #[test]
    fn duplicate_pure_helper_false_for_lowercase() {
        assert!(!is_duplicate_sqlstate("42p05"));
    }

    // ═════════════════════════════════════════════════════════════════════════
    // NEW TESTS: Pool State Machine (Stap 3)
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn pool_slot_initial_state_is_free() {
        let slot = PoolSlot::new();
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
        assert!(slot.conn.load(Ordering::Relaxed).is_null());
        assert_eq!(slot.owner_thread.load(Ordering::Relaxed), 0);
        assert_eq!(slot.generation.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pool_slot_claim_free_succeeds() {
        let slot = PoolSlot::new();
        assert!(slot.try_claim_free());
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RESERVED);
    }

    #[test]
    fn pool_slot_claim_free_fails_when_reserved() {
        let slot = PoolSlot::new();
        assert!(slot.try_claim_free());
        // Second claim must fail
        assert!(!slot.try_claim_free());
    }

    #[test]
    fn pool_slot_claim_free_fails_when_ready() {
        let slot = PoolSlot::new();
        slot.state.store(SLOT_READY, Ordering::Relaxed);
        assert!(!slot.try_claim_free());
    }

    #[test]
    fn pool_slot_mark_ready_after_reserve() {
        let slot = PoolSlot::new();
        assert!(slot.try_claim_free());
        slot.mark_ready();
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_READY);
    }

    #[test]
    fn pool_slot_mark_error_after_reserve() {
        let slot = PoolSlot::new();
        assert!(slot.try_claim_free());
        slot.mark_error();
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_ERROR);
    }

    #[test]
    fn pool_slot_begin_reconnect_from_ready() {
        let slot = PoolSlot::new();
        slot.state.store(SLOT_READY, Ordering::Relaxed);
        assert!(slot.try_begin_reconnect());
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RECONNECTING);
    }

    #[test]
    fn pool_slot_begin_reconnect_fails_from_free() {
        let slot = PoolSlot::new();
        assert!(!slot.try_begin_reconnect());
    }

    #[test]
    fn pool_slot_reclaim_error_succeeds() {
        let slot = PoolSlot::new();
        slot.state.store(SLOT_ERROR, Ordering::Relaxed);
        assert!(slot.try_reclaim_error());
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_RESERVED);
    }

    #[test]
    fn pool_slot_reclaim_error_fails_from_ready() {
        let slot = PoolSlot::new();
        slot.state.store(SLOT_READY, Ordering::Relaxed);
        assert!(!slot.try_reclaim_error());
    }

    #[test]
    fn pool_slot_reclaim_zombie_from_ready() {
        let slot = PoolSlot::new();
        slot.state.store(SLOT_READY, Ordering::Relaxed);
        assert!(slot.try_reclaim_zombie());
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    }

    #[test]
    fn pool_slot_reclaim_zombie_fails_from_free() {
        let slot = PoolSlot::new();
        assert!(!slot.try_reclaim_zombie());
    }

    #[test]
    fn pool_slot_release_clears_owner_and_state() {
        let slot = PoolSlot::new();
        slot.state.store(SLOT_READY, Ordering::Relaxed);
        slot.owner_thread.store(12345, Ordering::Relaxed);
        slot.release();
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
        assert_eq!(slot.owner_thread.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pool_slot_full_lifecycle() {
        // FREE → RESERVED → READY → RECONNECTING → READY → FREE
        let slot = PoolSlot::new();
        assert!(slot.try_claim_free()); // FREE → RESERVED
        slot.mark_ready(); // RESERVED → READY
        assert!(slot.try_begin_reconnect()); // READY → RECONNECTING
        slot.mark_ready(); // RECONNECTING → READY
        slot.release(); // READY → FREE
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    }

    #[test]
    fn pool_slot_error_recovery_lifecycle() {
        // FREE → RESERVED → ERROR → RESERVED → READY → FREE
        let slot = PoolSlot::new();
        assert!(slot.try_claim_free()); // FREE → RESERVED
        slot.mark_error(); // RESERVED → ERROR
        assert!(slot.try_reclaim_error()); // ERROR → RESERVED
        slot.mark_ready(); // RESERVED → READY
        slot.release(); // READY → FREE
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    }

    #[test]
    fn pool_slot_concurrent_claim_only_one_wins() {
        use std::sync::Arc;
        let slot = Arc::new(PoolSlot::new());
        let mut handles = vec![];
        let wins = Arc::new(AtomicU32::new(0));

        for _ in 0..10 {
            let s = Arc::clone(&slot);
            let w = Arc::clone(&wins);
            handles.push(std::thread::spawn(move || {
                if s.try_claim_free() {
                    w.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(wins.load(Ordering::Relaxed), 1);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // TLS Pool Cache
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn tls_pool_cache_initially_empty() {
        tls_pool_cache_clear();
        assert!(tls_pool_cache_get(0x1234).is_none());
    }

    #[test]
    fn tls_pool_cache_set_and_get() {
        tls_pool_cache_set(0xABCD, 5, 42);
        let result = tls_pool_cache_get(0xABCD);
        assert_eq!(result, Some((5, 42)));
    }

    #[test]
    fn tls_pool_cache_miss_for_different_db() {
        tls_pool_cache_set(0xABCD, 5, 42);
        assert!(tls_pool_cache_get(0x9999).is_none());
    }

    #[test]
    fn tls_pool_cache_clear_makes_miss() {
        tls_pool_cache_set(0xABCD, 5, 42);
        tls_pool_cache_clear();
        assert!(tls_pool_cache_get(0xABCD).is_none());
    }

    #[test]
    fn tls_pool_cache_overwrite() {
        tls_pool_cache_set(0xABCD, 5, 42);
        tls_pool_cache_set(0xABCD, 7, 99);
        assert_eq!(tls_pool_cache_get(0xABCD), Some((7, 99)));
    }

    #[test]
    fn tls_pool_cache_is_thread_local() {
        tls_pool_cache_clear();
        tls_pool_cache_set(0x1111, 3, 10);

        let result = std::thread::spawn(|| {
            // Other thread should not see our cache
            tls_pool_cache_get(0x1111)
        })
        .join()
        .unwrap();

        assert!(result.is_none());
        // Our thread still has it
        assert_eq!(tls_pool_cache_get(0x1111), Some((3, 10)));
    }

    #[test]
    fn tls_pool_cache_generation_detects_stale() {
        let pool = PoolManager::new(10);
        let fake_conn = 0xDEAD as *mut c_void;

        // Simulate: slot 3 is ready with generation 5
        pool.slots[3].conn.store(fake_conn, Ordering::Relaxed);
        pool.slots[3].state.store(SLOT_READY, Ordering::Relaxed);
        pool.slots[3].generation.store(5, Ordering::Relaxed);

        // Cache it in TLS
        tls_pool_cache_set(0xAAAA, 3, 5);

        // Verify fast path would succeed
        let (idx, gen) = tls_pool_cache_get(0xAAAA).unwrap();
        assert_eq!(idx, 3);
        assert_eq!(
            pool.slots[idx as usize].generation.load(Ordering::Acquire),
            gen
        );

        // Now simulate reaper bumping generation
        pool.slots[3].generation.fetch_add(1, Ordering::SeqCst);

        // TLS cache still returns the old generation
        let (idx, gen) = tls_pool_cache_get(0xAAAA).unwrap();
        // But the slot generation no longer matches → stale!
        assert_ne!(
            pool.slots[idx as usize].generation.load(Ordering::Acquire),
            gen
        );
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Connection Registry
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn registry_register_and_find() {
        let reg = ConnectionRegistry::new();
        reg.register(0x100, 0xAAA);
        assert_eq!(reg.find(0x100), Some(0xAAA));
    }

    #[test]
    fn registry_find_missing_returns_none() {
        let reg = ConnectionRegistry::new();
        assert_eq!(reg.find(0x100), None);
    }

    #[test]
    fn registry_unregister_removes() {
        let reg = ConnectionRegistry::new();
        reg.register(0x100, 0xAAA);
        assert_eq!(reg.unregister(0x100), Some(0xAAA));
        assert_eq!(reg.find(0x100), None);
    }

    #[test]
    fn registry_unregister_missing_returns_none() {
        let reg = ConnectionRegistry::new();
        assert_eq!(reg.unregister(0x100), None);
    }

    #[test]
    fn registry_multiple_entries() {
        let reg = ConnectionRegistry::new();
        reg.register(0x100, 0xAAA);
        reg.register(0x200, 0xBBB);
        reg.register(0x300, 0xCCC);
        assert_eq!(reg.find(0x100), Some(0xAAA));
        assert_eq!(reg.find(0x200), Some(0xBBB));
        assert_eq!(reg.find(0x300), Some(0xCCC));
        assert_eq!(reg.len(), 3);
    }

    #[test]
    fn registry_overwrite_existing() {
        let reg = ConnectionRegistry::new();
        reg.register(0x100, 0xAAA);
        reg.register(0x100, 0xBBB);
        assert_eq!(reg.find(0x100), Some(0xBBB));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn registry_clear_empties_all() {
        let reg = ConnectionRegistry::new();
        reg.register(0x100, 0xAAA);
        reg.register(0x200, 0xBBB);
        reg.clear();
        assert_eq!(reg.len(), 0);
        assert_eq!(reg.find(0x100), None);
    }

    #[test]
    fn registry_find_any_library() {
        let reg = ConnectionRegistry::new();
        reg.register(0x100, 0xAAA);
        reg.register(0x200, 0xBBB);
        // Predicate: "is library" if conn addr is 0xBBB
        let result = reg.find_any_library(|conn| conn == 0xBBB);
        assert_eq!(result, Some(0xBBB));
    }

    #[test]
    fn registry_find_any_library_none_match() {
        let reg = ConnectionRegistry::new();
        reg.register(0x100, 0xAAA);
        let result = reg.find_any_library(|_| false);
        assert_eq!(result, None);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Db-to-Pool Mapping
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn db_to_pool_assign_and_find() {
        let dtp = DbToPool::new();
        dtp.assign(0x100, 5);
        assert_eq!(dtp.find(0x100), Some(5));
    }

    #[test]
    fn db_to_pool_find_missing_returns_none() {
        let dtp = DbToPool::new();
        assert_eq!(dtp.find(0x100), None);
    }

    #[test]
    fn db_to_pool_release_removes() {
        let dtp = DbToPool::new();
        dtp.assign(0x100, 5);
        assert_eq!(dtp.release(0x100), Some(5));
        assert_eq!(dtp.find(0x100), None);
    }

    #[test]
    fn db_to_pool_multiple_handles_same_slot() {
        // Multiple sqlite3* handles can share a pool slot
        let dtp = DbToPool::new();
        dtp.assign(0x100, 5);
        dtp.assign(0x200, 5);
        assert_eq!(dtp.find(0x100), Some(5));
        assert_eq!(dtp.find(0x200), Some(5));
    }

    #[test]
    fn db_to_pool_clear() {
        let dtp = DbToPool::new();
        dtp.assign(0x100, 5);
        dtp.assign(0x200, 7);
        dtp.clear();
        assert_eq!(dtp.len(), 0);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Pool Manager
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn pool_manager_creates_slots() {
        let pm = PoolManager::new(10);
        assert_eq!(pm.pool_size(), 10);
        // slots.len() is always POOL_SIZE_MAX for auto-grow support
        assert_eq!(pm.slots.len(), POOL_SIZE_MAX);
        for slot in &pm.slots {
            assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
        }
    }

    #[test]
    fn pool_manager_validate_connection_found() {
        let pm = PoolManager::new(5);
        let fake_conn = 0xBEEF as *mut c_void;
        pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
        pm.slots[2].state.store(SLOT_READY, Ordering::Relaxed);
        assert!(pm.validate_connection(fake_conn));
    }

    #[test]
    fn pool_manager_validate_connection_not_found() {
        let pm = PoolManager::new(5);
        let fake_conn = 0xBEEF as *mut c_void;
        assert!(!pm.validate_connection(fake_conn));
    }

    #[test]
    fn pool_manager_validate_connection_not_ready() {
        let pm = PoolManager::new(5);
        let fake_conn = 0xBEEF as *mut c_void;
        pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
        pm.slots[2].state.store(SLOT_FREE, Ordering::Relaxed); // not READY
        assert!(!pm.validate_connection(fake_conn));
    }

    #[test]
    fn pool_manager_touch_connection() {
        let pm = PoolManager::new(5);
        let fake_conn = 0xBEEF as *mut c_void;
        pm.slots[1].conn.store(fake_conn, Ordering::Relaxed);
        pm.slots[1].last_used.store(100, Ordering::Relaxed);

        pm.touch_connection(fake_conn, 999);
        assert_eq!(pm.slots[1].last_used.load(Ordering::Relaxed), 999);
    }

    #[test]
    fn pool_manager_touch_unknown_conn_is_noop() {
        let pm = PoolManager::new(5);
        pm.touch_connection(0xBEEF as *const c_void, 999);
        // Should not panic or modify anything
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Global Atomics
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn global_metadata_id_default_zero() {
        let pm = PoolManager::new(1);
        assert_eq!(pm.global_metadata_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn global_metadata_id_set_and_get() {
        let pm = PoolManager::new(1);
        pm.global_metadata_id.store(12345, Ordering::Relaxed);
        assert_eq!(pm.global_metadata_id.load(Ordering::Relaxed), 12345);
    }

    #[test]
    fn global_last_insert_rowid_set_and_get() {
        let pm = PoolManager::new(1);
        pm.global_last_insert_rowid.store(67890, Ordering::Relaxed);
        assert_eq!(pm.global_last_insert_rowid.load(Ordering::Relaxed), 67890);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Fork Safety
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn pool_reset_for_child_clears_all_slots() {
        let pm = PoolManager::new(5);
        let fake_conn = 0xBEEF as *mut c_void;

        // Set up slot 2 as READY with a connection
        pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
        pm.slots[2].state.store(SLOT_READY, Ordering::Relaxed);
        pm.slots[2].owner_thread.store(999, Ordering::Relaxed);
        pm.slots[2].generation.store(5, Ordering::Relaxed);

        pm.registry.register(0x100, 0xAAA);
        pm.db_to_pool.assign(0x100, 2);

        pm.reset_for_child();

        // All slots should be FREE with null conn
        for slot in &pm.slots {
            assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
            assert!(slot.conn.load(Ordering::Relaxed).is_null());
            assert_eq!(slot.owner_thread.load(Ordering::Relaxed), 0);
        }
        // Generation should have been bumped
        assert!(pm.slots[2].generation.load(Ordering::Relaxed) > 5);

        // Registry and db_to_pool should be empty
        assert_eq!(pm.registry.len(), 0);
        assert_eq!(pm.db_to_pool.len(), 0);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Reaper
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn reaper_ignores_active_slots() {
        let pm = PoolManager::new(5);
        let fake = 0xBEEF as *mut c_void;
        pm.slots[0].conn.store(fake, Ordering::Relaxed);
        pm.slots[0].state.store(SLOT_READY, Ordering::Relaxed);
        pm.slots[0].last_used.store(0, Ordering::Relaxed);

        // Reaper should not touch READY slots
        let to_destroy = pm.reap_idle(10000);
        assert!(to_destroy.is_empty());
        assert_eq!(pm.slots[0].state.load(Ordering::Relaxed), SLOT_READY);
    }

    #[test]
    fn reaper_destroys_idle_free_slots() {
        let pm = PoolManager::new(5);
        pm.idle_timeout_secs.store(60, Ordering::Relaxed);
        let fake = 0xBEEF as *mut c_void;

        // Slot 0: FREE with connection, last used 100 seconds ago
        pm.slots[0].conn.store(fake, Ordering::Relaxed);
        pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
        pm.slots[0].last_used.store(100, Ordering::Relaxed);

        let to_destroy = pm.reap_idle(200); // 200 - 100 = 100 > 60 timeout
        assert_eq!(to_destroy.len(), 1);
        assert_eq!(to_destroy[0].0, 0); // slot index
        assert_eq!(to_destroy[0].1, fake); // connection pointer

        // Slot should now be FREE with null conn
        assert_eq!(pm.slots[0].state.load(Ordering::Relaxed), SLOT_FREE);
        assert!(pm.slots[0].conn.load(Ordering::Relaxed).is_null());
    }

    #[test]
    fn reaper_skips_recently_used() {
        let pm = PoolManager::new(5);
        pm.idle_timeout_secs.store(60, Ordering::Relaxed);
        let fake = 0xBEEF as *mut c_void;

        pm.slots[0].conn.store(fake, Ordering::Relaxed);
        pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
        pm.slots[0].last_used.store(180, Ordering::Relaxed);

        let to_destroy = pm.reap_idle(200); // 200 - 180 = 20 < 60 timeout
        assert!(to_destroy.is_empty());
        // Connection should still be there
        assert_eq!(pm.slots[0].conn.load(Ordering::Relaxed), fake);
    }

    #[test]
    fn reaper_skips_free_slot_without_conn() {
        let pm = PoolManager::new(5);
        pm.idle_timeout_secs.store(60, Ordering::Relaxed);
        // Slot 0: FREE, no connection
        pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
        pm.slots[0].last_used.store(0, Ordering::Relaxed);

        let to_destroy = pm.reap_idle(10000);
        assert!(to_destroy.is_empty());
    }

    #[test]
    fn reaper_bumps_generation_before_destroying() {
        let pm = PoolManager::new(5);
        pm.idle_timeout_secs.store(60, Ordering::Relaxed);
        let fake = 0xBEEF as *mut c_void;

        pm.slots[0].conn.store(fake, Ordering::Relaxed);
        pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
        pm.slots[0].last_used.store(0, Ordering::Relaxed);
        pm.slots[0].generation.store(10, Ordering::Relaxed);

        let _to_destroy = pm.reap_idle(10000);

        // Generation must have been incremented (invalidates TLS caches)
        assert!(pm.slots[0].generation.load(Ordering::Relaxed) > 10);
    }

    #[test]
    fn reaper_critical_fix_tls_generation_mismatch() {
        // This test verifies the fix for CRITICAL #1 + #2:
        // After reaping, a TLS-cached (slot_index, generation) pair must
        // fail validation because the generation was bumped.
        let pm = PoolManager::new(5);
        pm.idle_timeout_secs.store(60, Ordering::Relaxed);
        let fake = 0xBEEF as *mut c_void;

        pm.slots[2].conn.store(fake, Ordering::Relaxed);
        pm.slots[2].state.store(SLOT_FREE, Ordering::Relaxed);
        pm.slots[2].last_used.store(0, Ordering::Relaxed);
        pm.slots[2].generation.store(7, Ordering::Relaxed);

        // Simulate: thread cached this slot at generation 7
        let cached_gen = pm.slots[2].generation.load(Ordering::Acquire);
        assert_eq!(cached_gen, 7);

        // Reaper runs
        let _to_destroy = pm.reap_idle(10000);

        // Now the cached generation doesn't match → stale!
        let current_gen = pm.slots[2].generation.load(Ordering::Acquire);
        assert_ne!(cached_gen, current_gen, "Generation must change after reap");

        // The connection pointer is gone
        assert!(pm.slots[2].conn.load(Ordering::Relaxed).is_null());
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Prepared Statement Cache
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn stmt_cache_initial_empty() {
        let cache = StmtCache::new();
        assert_eq!(cache.count(), 0);
        assert!(cache.lookup(12345).is_none());
    }

    #[test]
    fn stmt_cache_add_and_lookup() {
        let mut cache = StmtCache::new();
        cache.add(12345, "ps_12345", 3, 1000);
        let entry = cache.lookup(12345).unwrap();
        assert_eq!(entry.sql_hash, 12345);
        assert_eq!(entry.stmt_name, "ps_12345");
        assert_eq!(entry.param_count, 3);
        assert_eq!(entry.last_used, 1000);
    }

    #[test]
    fn stmt_cache_lookup_miss() {
        let mut cache = StmtCache::new();
        cache.add(12345, "ps_12345", 3, 1000);
        assert!(cache.lookup(99999).is_none());
    }

    #[test]
    fn stmt_cache_lookup_zero_hash_returns_none() {
        let cache = StmtCache::new();
        assert!(cache.lookup(0).is_none());
    }

    #[test]
    fn stmt_cache_add_zero_hash_is_noop() {
        let mut cache = StmtCache::new();
        let evicted = cache.add(0, "ps_0", 0, 0);
        assert!(evicted.is_none());
        assert_eq!(cache.count(), 0);
    }

    #[test]
    fn stmt_cache_update_existing() {
        let mut cache = StmtCache::new();
        cache.add(12345, "ps_12345", 3, 1000);
        cache.add(12345, "ps_12345_v2", 5, 2000);
        let entry = cache.lookup(12345).unwrap();
        assert_eq!(entry.stmt_name, "ps_12345_v2");
        assert_eq!(entry.param_count, 5);
        assert_eq!(entry.last_used, 2000);
        assert_eq!(cache.count(), 1); // count should not increase
    }

    #[test]
    fn stmt_cache_multiple_entries() {
        let mut cache = StmtCache::new();
        for i in 1..=10u64 {
            cache.add(i, &format!("ps_{}", i), i as i32, i as i64);
        }
        assert_eq!(cache.count(), 10);
        for i in 1..=10u64 {
            let entry = cache.lookup(i).unwrap();
            assert_eq!(entry.sql_hash, i);
        }
    }

    #[test]
    fn stmt_cache_clear_returns_names() {
        let mut cache = StmtCache::new();
        cache.add(1, "ps_a", 1, 100);
        cache.add(2, "ps_b", 2, 200);
        cache.add(3, "ps_c", 3, 300);

        let evicted = cache.clear();
        assert_eq!(evicted.len(), 3);
        assert!(evicted.contains(&"ps_a".to_string()));
        assert!(evicted.contains(&"ps_b".to_string()));
        assert!(evicted.contains(&"ps_c".to_string()));
        assert_eq!(cache.count(), 0);
    }

    #[test]
    fn stmt_cache_clear_empty_returns_empty() {
        let mut cache = StmtCache::new();
        let evicted = cache.clear();
        assert!(evicted.is_empty());
    }

    #[test]
    fn stmt_cache_eviction_when_full() {
        let mut cache = StmtCache::new();
        // Fill the cache with STMT_CACHE_SIZE entries
        for i in 1..=STMT_CACHE_SIZE as u64 {
            cache.add(i, &format!("ps_{}", i), 1, i as i64);
        }
        assert_eq!(cache.count(), STMT_CACHE_SIZE);

        // Adding one more should evict the LRU (smallest last_used)
        let evicted = cache.add(99999, "ps_new", 1, 99999);
        assert!(evicted.is_some());
        // The evicted entry should be ps_1 (last_used=1, the oldest)
        assert_eq!(evicted.unwrap(), "ps_1");
    }

    #[test]
    fn stmt_cache_linear_probing_handles_collision() {
        let mut cache = StmtCache::new();
        // Two hashes that map to the same bucket (same lower bits)
        let h1 = 1u64;
        let h2 = 1 + STMT_CACHE_SIZE as u64; // same bucket
        cache.add(h1, "ps_a", 1, 100);
        cache.add(h2, "ps_b", 2, 200);

        assert_eq!(cache.lookup(h1).unwrap().stmt_name, "ps_a");
        assert_eq!(cache.lookup(h2).unwrap().stmt_name, "ps_b");
    }
}
