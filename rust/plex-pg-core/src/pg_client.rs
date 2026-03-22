/// Module: pg_client
///
/// Pool manager, registry, and prepared-statement cache logic extracted
/// from `pg_client.c`. libpq calls are routed through Rust wrappers so
/// the C side can stay as a thin shim.
///
/// FFI surface (called from `src/pg_client.c`):
///   rust_hash_sql(sql)              → u64   FNV-1a hash for prepared-stmt cache keys
///   rust_is_stale_sqlstate(s)       → i32   1 if SQLSTATE == "26000"
///   rust_is_duplicate_sqlstate(s)   → i32   1 if SQLSTATE == "42P05"
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

use crate::db_interpose_conn_utils::{log_debug, log_error, log_info, PthreadMutexGuard};
use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::ffi_types::{sqlite3, PgConnection};
use crate::libpq_helpers::{
    rust_pg_align_idle_timeout_with_server, rust_pg_probe_max_connections, rust_pq_clear,
    rust_pq_connectdb, rust_pq_error_message, rust_pq_exec, rust_pq_finish, rust_pq_reset,
    rust_pq_result_error_message, rust_pq_result_status, rust_pq_socket, rust_pq_status,
    rust_pq_transaction_status, PGconn, PGresult,
};
pub use crate::pg_client_stmt_cache::{
    rust_stmt_cache_add, rust_stmt_cache_clear, rust_stmt_cache_clear_local,
    rust_stmt_cache_drop, rust_stmt_cache_lookup,
};

const PG_DIAG_SQLSTATE: c_int = b'C' as c_int;

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

pub(crate) const POOL_SIZE_DEFAULT: usize = 50;

// ─── Pool Slot ───────────────────────────────────────────────────────────────

use std::collections::HashMap;
use std::sync::atomic::{
    AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Mutex, Once, OnceLock};

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
    let _ = TLS_POOL_CACHE.try_with(|c| {
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
    TLS_POOL_CACHE
        .try_with(|c| {
        let cache = c.get();
        if cache.db_handle == db_handle && !cache.is_empty() {
            Some((cache.slot_index, cache.generation))
        } else {
            None
        }
    })
        .ok()
        .flatten()
}

/// Invalidate the TLS pool cache.
pub(crate) fn tls_pool_cache_clear() {
    let _ = TLS_POOL_CACHE.try_with(|c| c.set(TlsPoolCache::EMPTY));
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

    pub fn drain_all(&self) -> Vec<usize> {
        let mut map = self.map.lock().unwrap();
        let conns: Vec<usize> = map.values().copied().collect();
        map.clear();
        conns
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

// ─── Pool Manager ────────────────────────────────────────────────────────────

/// Central pool manager holding all connection pool state.
pub(crate) struct PoolManager {
    pub slots: Vec<PoolSlot>,
    pub configured_size: AtomicUsize,
    pub configured_max_size: AtomicUsize,
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
    pub fn new(pool_size: usize, pool_max: usize) -> Self {
        // Allocate slots up to runtime max so auto-grow can expand
        // without reallocation. Only `configured_size` limits active use.
        let effective_max = pool_max.max(1);
        let mut slots = Vec::with_capacity(effective_max);
        for _ in 0..effective_max {
            slots.push(PoolSlot::new());
        }
        let effective_size = pool_size.clamp(1, effective_max);
        Self {
            slots,
            configured_size: AtomicUsize::new(effective_size),
            configured_max_size: AtomicUsize::new(effective_max),
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
        self.configured_size
            .load(Ordering::Relaxed)
            .min(self.pool_max())
    }

    /// Get configured maximum pool size (runtime cap).
    pub fn pool_max(&self) -> usize {
        self.configured_max_size.load(Ordering::Relaxed)
    }

    /// Check if a connection pointer is in any pool slot.
    pub fn validate_connection(&self, conn_ptr: *const c_void) -> bool {
        let size = self.pool_size();
        for i in 0..size {
            let slot = &self.slots[i];
            if slot.conn.load(Ordering::Acquire) == conn_ptr as *mut c_void
                && slot.state.load(Ordering::Acquire) == SLOT_READY
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
    POOL.get_or_init(|| PoolManager::new(POOL_SIZE_DEFAULT, POOL_SIZE_DEFAULT))
}

// ─── Logging helpers ─────────────────────────────────────────────────────────

// ─── Config Helpers ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct ConnConfig {
    host: String,
    port: i32,
    database: String,
    user: String,
    password: String,
    schema: String,
}

static CONN_CONFIG: OnceLock<ConnConfig> = OnceLock::new();
static CLIENT_INIT: Once = Once::new();

fn load_conn_config() -> ConnConfig {
    fn env(name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }
    let port = std::env::var("PLEX_PG_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(0);
    ConnConfig {
        host: env("PLEX_PG_HOST"),
        port,
        database: env("PLEX_PG_DATABASE"),
        user: env("PLEX_PG_USER"),
        password: env("PLEX_PG_PASSWORD"),
        schema: env("PLEX_PG_SCHEMA"),
    }
}

fn conn_config() -> &'static ConnConfig {
    CONN_CONFIG.get_or_init(load_conn_config)
}

fn parse_positive_env_or_default(name: &str, default_value: i32) -> i32 {
    match std::env::var(name) {
        Ok(v) => v
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(default_value),
        Err(_) => default_value,
    }
}

fn env_nonzero(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

fn write_str_to_cbuf(buf: &mut [c_char], src: &str) {
    let bytes = src.as_bytes();
    let len = bytes.len().min(buf.len().saturating_sub(1));
    for i in 0..len {
        buf[i] = bytes[i] as c_char;
    }
    if !buf.is_empty() {
        buf[len] = 0;
    }
}

fn cbuf_to_string(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

// ─── Thread Helpers ─────────────────────────────────────────────────────────

fn pthread_to_u64(t: libc::pthread_t) -> u64 {
    let mut out: u64 = 0;
    let n = std::cmp::min(
        std::mem::size_of::<libc::pthread_t>(),
        std::mem::size_of::<u64>(),
    );
    unsafe {
        std::ptr::copy_nonoverlapping(
            &t as *const _ as *const u8,
            &mut out as *mut _ as *mut u8,
            n,
        );
    }
    out
}

fn u64_to_pthread(id: u64) -> libc::pthread_t {
    let mut t: libc::pthread_t = unsafe { std::mem::zeroed() };
    let n = std::cmp::min(
        std::mem::size_of::<libc::pthread_t>(),
        std::mem::size_of::<u64>(),
    );
    unsafe {
        std::ptr::copy_nonoverlapping(
            &id as *const _ as *const u8,
            &mut t as *mut _ as *mut u8,
            n,
        );
    }
    t
}

fn current_thread_id() -> u64 {
    pthread_to_u64(unsafe { libc::pthread_self() })
}

fn threads_equal(a: u64, b: u64) -> bool {
    if a == 0 || b == 0 {
        return false;
    }
    unsafe { libc::pthread_equal(u64_to_pthread(a), u64_to_pthread(b)) != 0 }
}

fn check_thread_alive(thread_id: u64) -> bool {
    if thread_id == 0 {
        return false;
    }
    unsafe { libc::pthread_kill(u64_to_pthread(thread_id), 0) == 0 }
}

fn sleep_ms(ms: i32) {
    if ms <= 0 {
        return;
    }
    unsafe {
        libc::usleep((ms as u32).saturating_mul(1000));
    }
}

// ─── Connection Helpers ─────────────────────────────────────────────────────

const PG_SOCKET_TIMEOUT_SEC: i64 = 60;
const CONNECTION_OK: i32 = 0;
const PGRES_COMMAND_OK: i32 = 1;
const PGRES_TUPLES_OK: i32 = 2;

fn conn_db_path(conn: *mut PgConnection) -> String {
    if conn.is_null() {
        return String::new();
    }
    unsafe { cbuf_to_string(&(*conn).db_path) }
}

fn conn_is_pg_active(conn: *mut PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    unsafe { (*conn).is_pg_active != 0 }
}

fn conn_is_streaming_active(conn: *mut PgConnection) -> bool {
    if conn.is_null() {
        return false;
    }
    unsafe { (*conn).streaming_active.load(Ordering::Relaxed) != 0 }
}

fn pg_set_socket_timeout(pg_conn: *mut PGconn) {
    if pg_conn.is_null() {
        return;
    }
    let sock = rust_pq_socket(pg_conn);
    if sock < 0 {
        log_error("pg_set_socket_timeout: invalid socket");
        return;
    }

    let tv = libc::timeval {
        tv_sec: PG_SOCKET_TIMEOUT_SEC,
        tv_usec: 0,
    };
    unsafe {
        if libc::setsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        ) < 0
        {
            log_error("pg_set_socket_timeout: failed to set SO_RCVTIMEO");
        }
        if libc::setsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        ) < 0
        {
            log_error("pg_set_socket_timeout: failed to set SO_SNDTIMEO");
        }
    }

    log_debug(&format!(
        "Socket timeout set to {} seconds for socket {}",
        PG_SOCKET_TIMEOUT_SEC, sock
    ));
}

fn exec_command(pg_conn: *mut PGconn, sql: &str) -> bool {
    if pg_conn.is_null() {
        return false;
    }
    let cs = match CString::new(sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let res = rust_pq_exec(pg_conn, cs.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_COMMAND_OK;
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}

fn exec_tuples(pg_conn: *mut PGconn, sql: &str) -> bool {
    if pg_conn.is_null() {
        return false;
    }
    let cs = match CString::new(sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let res = rust_pq_exec(pg_conn, cs.as_ptr());
    let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_TUPLES_OK;
    if !res.is_null() {
        rust_pq_clear(res);
    }
    ok
}

fn apply_session_settings(pg_conn: *mut PGconn, schema: &str, deallocate_all: bool) {
    if pg_conn.is_null() {
        return;
    }

    let schema_cmd = format!("SET search_path TO {}, public", schema);
    let res = match CString::new(schema_cmd) {
        Ok(s) => rust_pq_exec(pg_conn, s.as_ptr()),
        Err(_) => std::ptr::null_mut(),
    };
    if res.is_null() || rust_pq_result_status(res) != PGRES_COMMAND_OK {
        let err = if res.is_null() {
            "<null result>".to_string()
        } else {
            unsafe {
                let msg = rust_pq_result_error_message(res);
                if msg.is_null() {
                    "<null>".to_string()
                } else {
                    CStr::from_ptr(msg).to_string_lossy().into_owned()
                }
            }
        };
        log_error(&format!("Failed to set search_path: {}", err));
    }
    if !res.is_null() {
        rust_pq_clear(res);
    }

    // Deallocate any leftover prepared statements from previous shim instance
    if deallocate_all {
        let _ = exec_command(pg_conn, "DEALLOCATE ALL");
    }

    if !exec_command(pg_conn, "SET statement_timeout = '60s'") {
        log_error("Failed to set statement_timeout");
    }
}

fn build_conninfo(cfg: &ConnConfig, with_keepalives: bool) -> String {
    if with_keepalives {
        format!(
            "host={} port={} dbname={} user={} password={} \
             connect_timeout=5 keepalives=1 keepalives_idle=30 \
             keepalives_interval=10 keepalives_count=3",
            cfg.host, cfg.port, cfg.database, cfg.user, cfg.password
        )
    } else {
        format!(
            "host={} port={} dbname={} user={} password={} connect_timeout=5",
            cfg.host, cfg.port, cfg.database, cfg.user, cfg.password
        )
    }
}

fn create_connection_struct(db_path: &str, shadow_db: *mut crate::ffi_types::sqlite3) -> *mut PgConnection {
    unsafe {
        let conn_ptr = libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection;
        if conn_ptr.is_null() {
            log_error("Failed to allocate pg_connection_t");
            return std::ptr::null_mut();
        }
        if libc::pthread_mutex_init(&mut (*conn_ptr).mutex as *mut _, std::ptr::null()) != 0 {
            log_error("pthread_mutex_init failed for pg_connection_t");
            libc::free(conn_ptr as *mut libc::c_void);
            return std::ptr::null_mut();
        }
        (*conn_ptr).shadow_db = shadow_db;
        write_str_to_cbuf(&mut (*conn_ptr).db_path, db_path);
        conn_ptr
    }
}

fn destroy_connection_struct(conn: *mut PgConnection) {
    if conn.is_null() {
        return;
    }
    unsafe {
        libc::pthread_mutex_destroy(&mut (*conn).mutex as *mut _);
        libc::free(conn as *mut libc::c_void);
    }
}

fn create_pool_connection(db_path: *const c_char) -> *mut c_void {
    let cfg = conn_config();
    log_debug(&format!(
        "create_pool_connection: host='{}' port={} db='{}' user='{}' schema='{}'",
        cfg.host, cfg.port, cfg.database, cfg.user, cfg.schema
    ));

    if cfg.host.is_empty() || cfg.port == 0 {
        log_error(&format!(
            "Pool connection skipped: config not loaded (host='{}' port={}). Check PLEX_PG_HOST/PLEX_PG_PORT env vars.",
            cfg.host, cfg.port
        ));
        return std::ptr::null_mut();
    }

    let db_path_str = if db_path.is_null() {
        ""
    } else {
        unsafe { cstr_to_str_or_empty(db_path) }
    };

    let conn_ptr = create_connection_struct(db_path_str, std::ptr::null_mut());
    if conn_ptr.is_null() {
        return std::ptr::null_mut();
    }

    let conninfo = build_conninfo(cfg, true);
    let conninfo_c = match CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => {
            destroy_connection_struct(conn_ptr);
            return std::ptr::null_mut();
        }
    };

    unsafe {
        (*conn_ptr).conn = rust_pq_connectdb(conninfo_c.as_ptr());
        if rust_pq_status((*conn_ptr).conn) != CONNECTION_OK {
            let err = if (*conn_ptr).conn.is_null() {
                "<null connection>".to_string()
            } else {
                let msg = rust_pq_error_message((*conn_ptr).conn);
                if msg.is_null() {
                    "<null>".to_string()
                } else {
                    CStr::from_ptr(msg).to_string_lossy().into_owned()
                }
            };
            log_error(&format!("Pool connection failed: {}", err));
            if !(*conn_ptr).conn.is_null() {
                rust_pq_finish((*conn_ptr).conn);
            }
            (*conn_ptr).conn = std::ptr::null_mut();
        } else {
            pg_set_socket_timeout((*conn_ptr).conn);
            apply_session_settings((*conn_ptr).conn, &cfg.schema, true);
            (*conn_ptr).is_pg_active = 1;
        }
    }

    conn_ptr as *mut c_void
}

fn destroy_pool_connection(conn: *mut c_void) {
    let conn = conn as *mut PgConnection;
    if conn.is_null() {
        return;
    }
    rust_stmt_cache_drop(conn as *mut c_void);
    unsafe {
        if !(*conn).conn.is_null() {
            rust_pq_finish((*conn).conn);
        }
    }
    destroy_connection_struct(conn);
}

fn check_conn_ok(conn: *mut c_void) -> bool {
    let conn = conn as *mut PgConnection;
    if conn.is_null() {
        return false;
    }
    unsafe { !(*conn).conn.is_null() && rust_pq_status((*conn).conn) == CONNECTION_OK }
}

fn reset_conn(conn: *mut c_void) -> bool {
    let conn = conn as *mut PgConnection;
    if conn.is_null() {
        return false;
    }
    unsafe {
        if (*conn).conn.is_null() {
            return false;
        }
        rust_pq_reset((*conn).conn);
        if rust_pq_status((*conn).conn) != CONNECTION_OK {
            return false;
        }
        pg_set_socket_timeout((*conn).conn);
        let cfg = conn_config();
        apply_session_settings((*conn).conn, &cfg.schema, false);
    }
    true
}

fn reconnect_conn(conn: *mut c_void) -> bool {
    let conn = conn as *mut PgConnection;
    if conn.is_null() {
        return false;
    }
    let cfg = conn_config();
    let conninfo = build_conninfo(cfg, true);
    let conninfo_c = match CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let _conn_guard = PthreadMutexGuard::lock(&mut (*conn).mutex as *mut _);
        if !(*conn).conn.is_null() {
            rust_pq_finish((*conn).conn);
            (*conn).conn = std::ptr::null_mut();
        }

        let new_pg = rust_pq_connectdb(conninfo_c.as_ptr());
        if rust_pq_status(new_pg) == CONNECTION_OK {
            pg_set_socket_timeout(new_pg);
            apply_session_settings(new_pg, &cfg.schema, false);
            (*conn).conn = new_pg;
            (*conn).is_pg_active = 1;
            return true;
        }

        let err = if new_pg.is_null() {
            "<null connection>".to_string()
        } else {
            let msg = rust_pq_error_message(new_pg);
            if msg.is_null() {
                "<null>".to_string()
            } else {
                CStr::from_ptr(msg).to_string_lossy().into_owned()
            }
        };
        log_error(&format!("Pool: reconnect failed: {}", err));
        if !new_pg.is_null() {
            rust_pq_finish(new_pg);
        }
        (*conn).conn = std::ptr::null_mut();
        (*conn).is_pg_active = 0;
        false
    }
}

fn get_txn_status(conn: *mut c_void) -> i32 {
    let conn = conn as *mut PgConnection;
    if conn.is_null() {
        return 0;
    }
    unsafe {
        if (*conn).conn.is_null() {
            return 0;
        }
        rust_pq_transaction_status((*conn).conn)
    }
}

fn exec_simple(conn: *mut c_void, sql: *const c_char) -> bool {
    let conn = conn as *mut PgConnection;
    if conn.is_null() || sql.is_null() {
        return false;
    }
    let s = unsafe { cstr_to_str_or_empty(sql) };
    let trimmed = s.trim_start();
    let lower = trimmed.to_ascii_lowercase();

    if lower.starts_with("commit") || lower.starts_with("rollback") || lower.starts_with("end") {
        let txn = unsafe {
            if (*conn).conn.is_null() {
                0
            } else {
                rust_pq_transaction_status((*conn).conn)
            }
        };
        if txn != PQTRANS_INTRANS && txn != PQTRANS_INERROR {
            log_debug(&format!(
                "exec_simple: skipped {} in non-transaction state={}",
                trimmed, txn
            ));
            return true;
        }
    }

    let cmd = match CString::new(trimmed) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        if (*conn).conn.is_null() {
            return false;
        }
        let res = rust_pq_exec((*conn).conn, cmd.as_ptr());
        let ok = !res.is_null() && rust_pq_result_status(res) == PGRES_COMMAND_OK;
        if !res.is_null() {
            rust_pq_clear(res);
        }
        ok
    }
}

fn close_handle_connection(conn: *mut PgConnection) {
    if conn.is_null() {
        return;
    }
    rust_stmt_cache_clear(conn as *mut c_void);
    rust_stmt_cache_drop(conn as *mut c_void);
    unsafe {
        let _conn_guard = PthreadMutexGuard::lock(&mut (*conn).mutex as *mut _);
        if !(*conn).conn.is_null() {
            rust_pq_finish((*conn).conn);
            (*conn).conn = std::ptr::null_mut();
        }
    }
    destroy_connection_struct(conn);
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

#[inline]
fn retry_count_get() -> i32 {
    POOL_RETRY_COUNT.try_with(|c| c.get()).unwrap_or(0)
}

#[inline]
fn retry_count_set(v: i32) {
    let _ = POOL_RETRY_COUNT.try_with(|c| c.set(v));
}

// ─── Pool Get Connection: 7-Phase Algorithm ──────────────────────────────────
//
// This is the core pool algorithm, migrated from C's pool_get_connection().
// All libpq operations are performed via Rust helpers; state management
// (slot claiming, TLS cache, generation checks) stays in Rust.

/// Internal: full 7-phase pool acquisition.
/// Returns an opaque pg_connection_t* or null.
fn pool_get_connection_inner(db_path: *const c_char) -> *mut c_void {
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

    let current_thread = current_thread_id();
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
                if threads_equal(owner, current_thread) {
                    let conn = slot.conn.load(Ordering::Acquire);
                    if !conn.is_null() && check_conn_ok(conn) {
                        // Skip if streaming
                        if conn_is_streaming_active(conn as *mut PgConnection) {
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
        if check_thread_alive(owner) {
            continue; // Thread alive — don't touch
        }

        // Thread is dead → safe to reclaim, unless streaming
        let conn = slot.conn.load(Ordering::Acquire);
        if !conn.is_null() && conn_is_streaming_active(conn as *mut PgConnection) {
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
                destroy_pool_connection(conn_ptr);
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
        if !threads_equal(owner, current_thread) {
            continue;
        }

        let conn = slot.conn.load(Ordering::Acquire);
        if !conn.is_null() && check_conn_ok(conn) {
            // Skip streaming connections
            if conn_is_streaming_active(conn as *mut PgConnection) {
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
            rust_stmt_cache_clear(conn);
            let ok = reconnect_conn(conn);
            if ok {
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
        if conn_is_streaming_active(conn as *mut PgConnection) {
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
        let txn = get_txn_status(conn);
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
            let _ = exec_simple(conn, cmd.as_ptr());
        }

        // Clear stmt cache and reset connection
        rust_stmt_cache_clear(conn);
        let reset_ok = reset_conn(conn);

        if reset_ok {
            log_debug(&format!("Pool: reusing reset connection in slot {}", i));
            slot.mark_ready();
            tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
            return conn;
        }

        // Reset failed — do full reconnect
        rust_stmt_cache_clear(conn);
        let reconn_ok = reconnect_conn(conn);
        if reconn_ok {
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

        let new_conn = create_pool_connection(db_path);
        if !new_conn.is_null() && conn_is_pg_active(new_conn as *mut PgConnection) {
            slot.conn.store(new_conn, Ordering::Release);
            log_info(&format!("Pool: created new connection in slot {}", i));
            slot.mark_ready();
            tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
            return new_conn;
        } else {
            // Creation failed — release slot
            log_error(&format!("Pool: failed to create connection for slot {}", i));
            if !new_conn.is_null() {
                destroy_pool_connection(new_conn);
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
            destroy_pool_connection(old_conn);
        }

        log_debug(&format!("Pool: reclaiming error slot {}", i));

        let new_conn = create_pool_connection(db_path);
        if !new_conn.is_null() && conn_is_pg_active(new_conn as *mut PgConnection) {
            slot.conn.store(new_conn, Ordering::Release);
            log_info(&format!("Pool: recovered slot {} with new connection", i));
            slot.mark_ready();
            tls_pool_cache_set(0, i as u32, slot.generation.load(Ordering::Acquire));
            return new_conn;
        } else {
            if !new_conn.is_null() {
                destroy_pool_connection(new_conn);
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
    let runtime_max = pm.pool_max();
    if current_size < runtime_max {
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

                    let new_conn = create_pool_connection(db_path);
                    if !new_conn.is_null()
                        && conn_is_pg_active(new_conn as *mut PgConnection)
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
                            destroy_pool_connection(new_conn);
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
    let retry_count = retry_count_get();

    let delays = crate::pg_config::get_retry_delays_vec();
    let max_retries = delays.len() as i32;

    if retry_count < max_retries {
        let delay = delays[retry_count as usize];
        log_error(&format!(
            "Pool: no connection available, retry {}/{} in {}ms",
            retry_count + 1,
            max_retries,
            delay
        ));
        retry_count_set(retry_count + 1);
        sleep_ms(delay);

        // Recursive retry
        let result = pool_get_connection_inner(db_path);
        if !result.is_null() {
            retry_count_set(0);
        }
        return result;
    }

    // All retries exhausted
    log_error(&format!(
        "Pool: no available slots after {} retries (all {} slots busy)",
        max_retries,
        pm.configured_size.load(Ordering::Relaxed)
    ));
    retry_count_set(0);
    std::ptr::null_mut()
}

// ─── Pool Release (close_for_db) ────────────────────────────────────────────

/// Release a pool slot when a database handle is closed.
/// The connection stays open in the pool for potential reuse.
fn pool_release_for_db_inner(db_handle: usize) {
    let pm = pool();

    // Remove db_to_pool mapping
    let slot_opt = pm.db_to_pool.release(db_handle);

    if let Some(slot_idx) = slot_opt {
        let pool_size = pm.pool_size();
        if slot_idx < pool_size {
            let slot = &pm.slots[slot_idx];
            let current_thread = current_thread_id();
            let owner = slot.owner_thread.load(Ordering::Acquire);

            if threads_equal(owner, current_thread) {
                let state = slot.state.load(Ordering::Acquire);
                if state == SLOT_READY {
                    // Commit/rollback pending transaction before release
                    let conn = slot.conn.load(Ordering::Acquire);
                    if !conn.is_null() {
                        let txn = get_txn_status(conn);
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
                            let _ = exec_simple(conn, cmd.as_ptr());
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

    // Check if connection is still OK
    if check_conn_ok(conn) {
        return 0; // Healthy
    }

    log_info("Pool: connection health check failed, resetting");

    let pm = pool();
    let current_thread = current_thread_id();
    let pool_size = pm.pool_size();

    for i in 0..pool_size {
        let slot = &pm.slots[i];
        if slot.conn.load(Ordering::Acquire) != conn {
            continue;
        }
        let owner = slot.owner_thread.load(Ordering::Acquire);
        if !threads_equal(owner, current_thread) {
            continue;
        }

        // Try READY → RECONNECTING
        if !slot.try_begin_reconnect() {
            break;
        }

        rust_stmt_cache_clear(conn);

        // Try PQreset first
        let reset_ok = reset_conn(conn);
        if reset_ok {
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
        let reconn_ok = reconnect_conn(conn);
        if reconn_ok {
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

    if !conn_is_pg_active(pool_conn as *mut PgConnection) {
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

/// Initialize the pool with optional pool_size and idle_timeout from env vars.
/// Called from pg_client_init().
#[no_mangle]
pub extern "C" fn rust_pool_init(pool_size: i32, pool_max: i32, idle_timeout: i32) {
    let requested_max = if pool_max > 0 {
        pool_max as usize
    } else {
        POOL_SIZE_DEFAULT
    };
    let requested_size = if pool_size > 0 {
        pool_size as usize
    } else {
        POOL_SIZE_DEFAULT
    };

    let pm = POOL.get_or_init(|| PoolManager::new(requested_size, requested_max));
    let slots_cap = pm.slots.len();
    let max_sz = requested_max.min(slots_cap).max(1);
    if requested_max > slots_cap {
        log_error(&format!(
            "Pool: requested max {} exceeds initialized slot capacity {}; clamping",
            requested_max, slots_cap
        ));
    }
    pm.configured_max_size.store(max_sz, Ordering::Relaxed);

    if requested_size <= max_sz {
        pm.configured_size.store(requested_size, Ordering::Relaxed);
    } else {
        pm.configured_size.store(max_sz, Ordering::Relaxed);
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
    let pm = pool();
    let pool_size = pm.pool_size();

    for i in 0..pool_size {
        let slot = &pm.slots[i];
        // Force to FREE
        slot.state.store(SLOT_FREE, Ordering::SeqCst);

        let conn = slot.conn.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !conn.is_null() {
            destroy_pool_connection(conn);
        }
        slot.owner_thread.store(0, Ordering::Release);
        slot.generation.store(0, Ordering::Release);
    }

    pm.db_to_pool.clear();
    pm.registry.clear();

    // Clear stmt caches
    crate::pg_client_stmt_cache::clear_all_stmt_caches();
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
    let pm = pool();

    // First try pool
    let lib_path = pm.library_db_path.lock().unwrap().clone();
    if let Some(path) = lib_path {
        if let Ok(cs) = std::ffi::CString::new(path) {
            let conn = pool_get_connection_inner(cs.as_ptr());
            if !conn.is_null() && conn_is_pg_active(conn as *mut PgConnection) {
                return conn;
            }
        }
    }

    // Fall back to registry: find any library connection
    pm.registry
        .find_any_library(|conn_ptr| {
            let conn = conn_ptr as *mut PgConnection;
            if !conn_is_pg_active(conn) {
                return false;
            }
            let path = conn_db_path(conn);
            is_library_db(&path)
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

// ═════════════════════════════════════════════════════════════════════════════
// Public C FFI — Client/Connection Operations
// ═════════════════════════════════════════════════════════════════════════════

/// Initialize client state and the connection pool.
#[no_mangle]
pub extern "C" fn rust_pg_client_init() {
    CLIENT_INIT.call_once(|| {
        let mut pool_size = parse_positive_env_or_default("PLEX_PG_POOL_SIZE", POOL_SIZE_DEFAULT as i32);
        let mut pool_max = parse_positive_env_or_default("PLEX_PG_POOL_MAX", pool_size);

        let cfg = conn_config();

        let db_max_connections = if cfg.host.is_empty() || cfg.port <= 0 {
            0
        } else {
            let host = CString::new(cfg.host.clone()).ok();
            let db = CString::new(cfg.database.clone()).ok();
            let user = CString::new(cfg.user.clone()).ok();
            let pass = CString::new(cfg.password.clone()).ok();
            if let (Some(h), Some(d), Some(u), Some(p)) = (host, db, user, pass) {
                rust_pg_probe_max_connections(h.as_ptr(), cfg.port, d.as_ptr(), u.as_ptr(), p.as_ptr())
            } else {
                0
            }
        };

        if db_max_connections > 0 {
            if pool_max != db_max_connections {
                log_info(&format!(
                    "Pool max ({}) does not match database max_connections ({}); adjusting to {}",
                    pool_max, db_max_connections, db_max_connections
                ));
                pool_max = db_max_connections;
            }
        } else {
            log_info(&format!(
                "Pool init: could not read database max_connections; keeping pool max={}",
                pool_max
            ));
        }

        if pool_size > pool_max {
            log_info(&format!(
                "Pool size {} exceeds pool max {}; clamping",
                pool_size, pool_max
            ));
            pool_size = pool_max;
        }

        let mut idle_timeout = 300;
        if let Ok(val) = std::env::var("PLEX_PG_IDLE_TIMEOUT") {
            if let Ok(v) = val.trim().parse::<i32>() {
                if v >= 10 {
                    idle_timeout = v;
                }
            }
        }

        if !cfg.host.is_empty() && cfg.port > 0 {
            let host = CString::new(cfg.host.clone()).ok();
            let db = CString::new(cfg.database.clone()).ok();
            let user = CString::new(cfg.user.clone()).ok();
            let pass = CString::new(cfg.password.clone()).ok();
            if let (Some(h), Some(d), Some(u), Some(p)) = (host, db, user, pass) {
                idle_timeout = rust_pg_align_idle_timeout_with_server(
                    idle_timeout,
                    h.as_ptr(),
                    cfg.port,
                    d.as_ptr(),
                    u.as_ptr(),
                    p.as_ptr(),
                );
            }
        }

        rust_pool_init(pool_size, pool_max, idle_timeout);

        log_info(&format!(
            "pg_client initialized (Rust pool): pool_size={}, pool_max={}, idle_timeout={}s",
            pool_size, pool_max, idle_timeout
        ));
    });
}

/// Cleanup client state and connection pool.
#[no_mangle]
pub extern "C" fn rust_pg_client_cleanup() {
    // Close any remaining handle connections
    let conns = pool().registry.drain_all();
    for conn in conns {
        close_handle_connection(conn as *mut PgConnection);
    }
    rust_pool_cleanup();
}

// ─── C ABI wrappers (pg_client.c replacement) ────────────────────────────────

#[no_mangle]
pub extern "C" fn pg_client_init() {
    rust_pg_client_init();
}

#[no_mangle]
pub extern "C" fn pg_client_cleanup() {
    rust_pg_client_cleanup();
}

#[no_mangle]
pub extern "C" fn pg_connect(db_path: *const c_char, shadow_db: *mut sqlite3) -> *mut PgConnection {
    rust_pg_connect(db_path, shadow_db)
}

#[no_mangle]
pub extern "C" fn pg_close(conn: *mut PgConnection) {
    rust_pg_close(conn);
}

#[no_mangle]
pub extern "C" fn pg_ensure_connection(conn: *mut PgConnection) -> c_int {
    rust_pg_ensure_connection(conn)
}

#[no_mangle]
pub extern "C" fn pg_register_connection(conn: *mut PgConnection) {
    rust_pg_register_connection(conn);
}

#[no_mangle]
pub extern "C" fn pg_unregister_connection(conn: *mut PgConnection) {
    rust_pg_unregister_connection(conn);
}

#[no_mangle]
pub extern "C" fn pg_find_connection(db: *mut sqlite3) -> *mut PgConnection {
    rust_pg_find_connection(db)
}

#[no_mangle]
pub extern "C" fn pg_find_handle_connection(db: *mut sqlite3) -> *mut PgConnection {
    rust_pg_find_handle_connection(db)
}

#[no_mangle]
pub extern "C" fn pg_find_any_library_connection() -> *mut PgConnection {
    rust_pg_find_any_library_connection()
}

#[no_mangle]
pub extern "C" fn pg_get_thread_connection(db_path: *const c_char) -> *mut PgConnection {
    unsafe { rust_pool_get_connection(db_path) as *mut PgConnection }
}

#[no_mangle]
pub extern "C" fn pg_pool_validate_connection(conn: *mut PgConnection) -> c_int {
    rust_pool_validate_connection(conn as *const c_void)
}

#[no_mangle]
pub extern "C" fn pg_pool_touch_connection(conn: *mut PgConnection) {
    rust_pool_touch_connection(conn as *const c_void);
}

#[no_mangle]
pub extern "C" fn pg_pool_check_connection_health(conn: *mut PgConnection) -> c_int {
    rust_pool_check_health(conn as *mut c_void)
}

#[no_mangle]
pub extern "C" fn pg_close_pool_for_db(db: *mut sqlite3) {
    if db.is_null() {
        return;
    }
    rust_pool_release_for_db(db as *const c_void);
}

#[no_mangle]
pub extern "C" fn pg_get_global_metadata_id() -> i64 {
    rust_get_global_metadata_id()
}

#[no_mangle]
pub extern "C" fn pg_set_global_metadata_id(id: i64) {
    rust_set_global_metadata_id(id);
}

#[no_mangle]
pub extern "C" fn pg_get_global_last_insert_rowid() -> i64 {
    rust_get_global_last_insert_rowid()
}

#[no_mangle]
pub extern "C" fn pg_set_global_last_insert_rowid(id: i64) {
    rust_set_global_last_insert_rowid(id);
}

#[no_mangle]
pub extern "C" fn pg_hash_sql(sql: *const c_char) -> u64 {
    rust_hash_sql(sql)
}

#[no_mangle]
pub extern "C" fn pg_stmt_cache_lookup(
    conn: *mut PgConnection,
    sql_hash: u64,
    stmt_name_out: *mut *const c_char,
) -> c_int {
    rust_stmt_cache_lookup(conn as *mut c_void, sql_hash, stmt_name_out)
}

#[no_mangle]
pub extern "C" fn pg_stmt_cache_add(
    conn: *mut PgConnection,
    sql_hash: u64,
    stmt_name: *const c_char,
    param_count: c_int,
) -> c_int {
    rust_stmt_cache_add(conn as *mut c_void, sql_hash, stmt_name, param_count)
}

#[no_mangle]
pub extern "C" fn pg_stmt_cache_clear(conn: *mut PgConnection) {
    rust_stmt_cache_clear(conn as *mut c_void);
}

#[no_mangle]
pub extern "C" fn pg_stmt_cache_clear_local(conn: *mut PgConnection) {
    rust_stmt_cache_clear_local(conn as *mut c_void);
}

#[no_mangle]
pub extern "C" fn pg_is_stale_prepared_stmt(res: *mut PGresult) -> c_int {
    if res.is_null() {
        return 0;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    rust_is_stale_sqlstate(sqlstate)
}

#[no_mangle]
pub extern "C" fn pg_is_duplicate_prepared_stmt(res: *mut PGresult) -> c_int {
    if res.is_null() {
        return 0;
    }
    let sqlstate = crate::libpq_helpers::rust_pq_result_error_field(res, PG_DIAG_SQLSTATE);
    rust_is_duplicate_sqlstate(sqlstate)
}

#[no_mangle]
pub extern "C" fn pg_pool_cleanup_after_fork() {
    rust_pool_cleanup_after_fork();
}

/// Connect a non-pooled PostgreSQL connection (for non-library DBs).
#[no_mangle]
pub extern "C" fn rust_pg_connect(
    db_path: *const c_char,
    shadow_db: *mut crate::ffi_types::sqlite3,
) -> *mut PgConnection {
    let db_path_str = if db_path.is_null() {
        ""
    } else {
        unsafe { cstr_to_str_or_empty(db_path) }
    };

    let conn_ptr = create_connection_struct(db_path_str, shadow_db);
    if conn_ptr.is_null() {
        return std::ptr::null_mut();
    }

    if is_library_db(db_path_str) {
        unsafe {
            (*conn_ptr).conn = std::ptr::null_mut();
            (*conn_ptr).is_pg_active = 1;
        }
        log_info(&format!("PostgreSQL pool-only connection for: {}", db_path_str));
        return conn_ptr;
    }

    let cfg = conn_config();
    let conninfo = build_conninfo(cfg, false);
    let conninfo_c = match CString::new(conninfo) {
        Ok(s) => s,
        Err(_) => return conn_ptr,
    };

    unsafe {
        (*conn_ptr).conn = rust_pq_connectdb(conninfo_c.as_ptr());
        if rust_pq_status((*conn_ptr).conn) != CONNECTION_OK {
            let err = if (*conn_ptr).conn.is_null() {
                "<null connection>".to_string()
            } else {
                let msg = rust_pq_error_message((*conn_ptr).conn);
                if msg.is_null() {
                    "<null>".to_string()
                } else {
                    CStr::from_ptr(msg).to_string_lossy().into_owned()
                }
            };
            log_error(&format!("PostgreSQL connection failed: {}", err));
            if !(*conn_ptr).conn.is_null() {
                rust_pq_finish((*conn_ptr).conn);
            }
            (*conn_ptr).conn = std::ptr::null_mut();
        } else {
            log_info(&format!("PostgreSQL connected for: {}", db_path_str));
            pg_set_socket_timeout((*conn_ptr).conn);
            apply_session_settings((*conn_ptr).conn, &cfg.schema, false);
            (*conn_ptr).is_pg_active = 1;
        }
    }

    conn_ptr
}

/// Ensure a non-pooled connection is live; reconnect if needed.
#[no_mangle]
pub extern "C" fn rust_pg_ensure_connection(conn: *mut PgConnection) -> i32 {
    if conn.is_null() {
        return 0;
    }

    unsafe {
        let mut conn_guard = PthreadMutexGuard::lock(&mut (*conn).mutex as *mut _);

        if !(*conn).conn.is_null() && rust_pq_status((*conn).conn) == CONNECTION_OK {
            if exec_tuples((*conn).conn, "SELECT 1") {
                conn_guard.unlock();
                return 1;
            }
            log_info("Connection health check failed, will reconnect");
        }

        if !(*conn).conn.is_null() {
            rust_pq_finish((*conn).conn);
            (*conn).conn = std::ptr::null_mut();
        }

        let cfg = conn_config();
        let conninfo = build_conninfo(cfg, false);
        let conninfo_c = match CString::new(conninfo) {
            Ok(s) => s,
            Err(_) => {
                (*conn).is_pg_active = 0;
                conn_guard.unlock();
                return 0;
            }
        };

        (*conn).conn = rust_pq_connectdb(conninfo_c.as_ptr());
        if rust_pq_status((*conn).conn) != CONNECTION_OK {
            let err = if (*conn).conn.is_null() {
                "<null connection>".to_string()
            } else {
                let msg = rust_pq_error_message((*conn).conn);
                if msg.is_null() {
                    "<null>".to_string()
                } else {
                    CStr::from_ptr(msg).to_string_lossy().into_owned()
                }
            };
            log_error(&format!("PostgreSQL reconnection failed: {}", err));
            if !(*conn).conn.is_null() {
                rust_pq_finish((*conn).conn);
            }
            (*conn).conn = std::ptr::null_mut();
            (*conn).is_pg_active = 0;
            conn_guard.unlock();
            return 0;
        }

        log_info("PostgreSQL reconnected successfully");
        pg_set_socket_timeout((*conn).conn);
        apply_session_settings((*conn).conn, &cfg.schema, false);
        (*conn).is_pg_active = 1;
        conn_guard.unlock();
        1
    }
}

/// Close and free a non-pooled connection.
#[no_mangle]
pub extern "C" fn rust_pg_close(conn: *mut PgConnection) {
    close_handle_connection(conn);
}

/// Register a non-pooled connection using its shadow DB handle.
#[no_mangle]
pub extern "C" fn rust_pg_register_connection(conn: *mut PgConnection) {
    if conn.is_null() {
        return;
    }
    unsafe {
        let db = (*conn).shadow_db;
        if db.is_null() {
            return;
        }
        pool().registry.register(db as usize, conn as usize);
    }
}

/// Unregister a non-pooled connection using its shadow DB handle.
#[no_mangle]
pub extern "C" fn rust_pg_unregister_connection(conn: *mut PgConnection) {
    if conn.is_null() {
        return;
    }
    unsafe {
        let db = (*conn).shadow_db;
        if db.is_null() {
            return;
        }
        pool().registry.unregister(db as usize);
    }
}

/// Find the handle connection for a sqlite3* handle.
#[no_mangle]
pub extern "C" fn rust_pg_find_handle_connection(
    db_handle: *const crate::ffi_types::sqlite3,
) -> *mut PgConnection {
    if db_handle.is_null() {
        return std::ptr::null_mut();
    }
    rust_find_registered_connection(db_handle as *const c_void) as *mut PgConnection
}

/// Find the active connection for a sqlite3* handle, including pool logic.
#[no_mangle]
pub extern "C" fn rust_pg_find_connection(
    db_handle: *const crate::ffi_types::sqlite3,
) -> *mut PgConnection {
    if db_handle.is_null() {
        return std::ptr::null_mut();
    }

    // Fork safety
    let _ = rust_pool_check_fork();

    let handle_conn = rust_find_registered_connection(db_handle as *const c_void) as *mut PgConnection;
    if handle_conn.is_null() {
        return std::ptr::null_mut();
    }

    let path = conn_db_path(handle_conn);

    if is_library_db(&path) {
        if env_nonzero("PLEX_PG_FORCE_SQLITE_LIBRARY") {
            return std::ptr::null_mut();
        }

        if env_nonzero("PLEX_PG_DISABLE_POOL") {
            if conn_is_pg_active(handle_conn) {
                return handle_conn;
            }
            return std::ptr::null_mut();
        }

        if let Ok(cs) = CString::new(path) {
            let pool_conn = pool_find_connection_for_db(db_handle as usize, cs.as_ptr());
            if !pool_conn.is_null() && conn_is_pg_active(pool_conn as *mut PgConnection) {
                return pool_conn as *mut PgConnection;
            }
        }

        log_debug("Pool full for library.db, falling back to SQLite");
        return std::ptr::null_mut();
    }

    if conn_is_pg_active(handle_conn) {
        handle_conn
    } else {
        std::ptr::null_mut()
    }
}

/// Find any active library connection (pool or handle).
#[no_mangle]
pub extern "C" fn rust_pg_find_any_library_connection() -> *mut PgConnection {
    rust_find_any_library_connection() as *mut PgConnection
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
        let pool = PoolManager::new(10, 64);
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
        let pm = PoolManager::new(10, 64);
        assert_eq!(pm.pool_size(), 10);
        // slots.len() tracks runtime max for auto-grow support
        assert_eq!(pm.slots.len(), 64);
        for slot in &pm.slots {
            assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
        }
    }

    #[test]
    fn pool_manager_validate_connection_found() {
        let pm = PoolManager::new(5, 64);
        let fake_conn = 0xBEEF as *mut c_void;
        pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
        pm.slots[2].state.store(SLOT_READY, Ordering::Relaxed);
        assert!(pm.validate_connection(fake_conn));
    }

    #[test]
    fn pool_manager_validate_connection_not_found() {
        let pm = PoolManager::new(5, 64);
        let fake_conn = 0xBEEF as *mut c_void;
        assert!(!pm.validate_connection(fake_conn));
    }

    #[test]
    fn pool_manager_validate_connection_not_ready() {
        let pm = PoolManager::new(5, 64);
        let fake_conn = 0xBEEF as *mut c_void;
        pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
        pm.slots[2].state.store(SLOT_FREE, Ordering::Relaxed); // not READY
        assert!(!pm.validate_connection(fake_conn));
    }

    #[test]
    fn pool_manager_touch_connection() {
        let pm = PoolManager::new(5, 64);
        let fake_conn = 0xBEEF as *mut c_void;
        pm.slots[1].conn.store(fake_conn, Ordering::Relaxed);
        pm.slots[1].last_used.store(100, Ordering::Relaxed);

        pm.touch_connection(fake_conn, 999);
        assert_eq!(pm.slots[1].last_used.load(Ordering::Relaxed), 999);
    }

    #[test]
    fn pool_manager_touch_unknown_conn_is_noop() {
        let pm = PoolManager::new(5, 64);
        pm.touch_connection(0xBEEF as *const c_void, 999);
        // Should not panic or modify anything
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Global Atomics
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn global_metadata_id_default_zero() {
        let pm = PoolManager::new(1, 64);
        assert_eq!(pm.global_metadata_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn global_metadata_id_set_and_get() {
        let pm = PoolManager::new(1, 64);
        pm.global_metadata_id.store(12345, Ordering::Relaxed);
        assert_eq!(pm.global_metadata_id.load(Ordering::Relaxed), 12345);
    }

    #[test]
    fn global_last_insert_rowid_set_and_get() {
        let pm = PoolManager::new(1, 64);
        pm.global_last_insert_rowid.store(67890, Ordering::Relaxed);
        assert_eq!(pm.global_last_insert_rowid.load(Ordering::Relaxed), 67890);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Fork Safety
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn pool_reset_for_child_clears_all_slots() {
        let pm = PoolManager::new(5, 64);
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
        let pm = PoolManager::new(5, 64);
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
        let pm = PoolManager::new(5, 64);
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
        let pm = PoolManager::new(5, 64);
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
        let pm = PoolManager::new(5, 64);
        pm.idle_timeout_secs.store(60, Ordering::Relaxed);
        // Slot 0: FREE, no connection
        pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
        pm.slots[0].last_used.store(0, Ordering::Relaxed);

        let to_destroy = pm.reap_idle(10000);
        assert!(to_destroy.is_empty());
    }

    #[test]
    fn reaper_bumps_generation_before_destroying() {
        let pm = PoolManager::new(5, 64);
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
        let pm = PoolManager::new(5, 64);
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

}
