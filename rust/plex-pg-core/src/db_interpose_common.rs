use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_long, c_uint, c_uchar, c_void};
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Once;

use crate::db_interpose_conn_utils::{log_debug, log_error, log_info, PthreadMutexGuard};
use crate::env_utils;
use crate::ffi_types::{sqlite3, sqlite3_stmt};

const EXC_QUERY_RING_SIZE: usize = 24;
const EXC_QUERY_MAX_LEN: usize = 320;
const EXC_PHASE_RING_SIZE: usize = 48;
const EXC_PHASE_MAX_LEN: usize = 32;

const CRASH_QUERY_MAX_LEN: usize = 512;
const CRASH_PHASE_MAX_LEN: usize = 64;

const WORK_NONE: c_int = 0;
const WORK_PREPARE_V2: c_int = 1;
const WORK_SHUTDOWN: c_int = 2;

const SQLITE_ERROR: c_int = 1;

const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;
const MAX_FAKE_VALUES: usize = 4096;
const PG_FAKE_VALUE_MAGIC: u32 = 0x50475641;

const MAX_EXCEPTION_TYPES: usize = 64;
const MAX_LOGGED_PER_TYPE: c_int = 3;
const MAX_LOGGED_TOTAL: c_int = 50;

const BOX_INNER_WIDTH: usize = 78;
const BOX_TL: &[u8] = b"\xE2\x95\x94"; // ╔
const BOX_TR: &[u8] = b"\xE2\x95\x97"; // ╗
const BOX_BL: &[u8] = b"\xE2\x95\x9A"; // ╚
const BOX_BR: &[u8] = b"\xE2\x95\x9D"; // ╝
const BOX_H: &[u8] = b"\xE2\x95\x90"; // ═
const BOX_ML: &[u8] = b"\xE2\x95\xA0"; // ╠
const BOX_MR: &[u8] = b"\xE2\x95\xA3"; // ╣

const UNKNOWN_STR: &[u8] = b"unknown\0";
const TRACE_LAST_QUERY_DEFAULT: &[u8] = b"/tmp/plex_pg_last_query.log\0";

type CollationCompare =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int>;
type CollationDestroy = Option<unsafe extern "C" fn(*mut c_void)>;

#[repr(C)]
struct TlsState {
    in_interpose_call: c_int,
    prepare_v2_depth: c_int,
    in_resolve_tables: c_int,
    value_type_calls: c_long,
    column_type_calls: c_long,
    last_query: *const c_char,
}

static TLS_INIT: Once = Once::new();
static mut TLS_KEY: libc::pthread_key_t = 0;
static mut TLS_FALLBACK: TlsState = TlsState {
    in_interpose_call: 0,
    prepare_v2_depth: 0,
    in_resolve_tables: 0,
    value_type_calls: 0,
    column_type_calls: 0,
    last_query: ptr::null(),
};

macro_rules! load_sym {
    ($slot:ident, $handle:expr, $name:expr) => {{
        let slot = ptr::addr_of_mut!($slot);
        if (*slot).is_none() {
            let sym = libc::dlsym($handle, $name.as_ptr() as *const c_char);
            if !sym.is_null() {
                *slot = Some(std::mem::transmute(sym));
            }
        }
    }};
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ExceptionTypeTracker {
    type_name: *const c_char,
    count: c_int,
    logged_with_trace: c_int,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct PgFakeValue {
    magic: u32,
    pg_stmt: *mut c_void,
    col_idx: c_int,
    row_idx: c_int,
    owner_thread: libc::pthread_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WorkerRequest {
    type_: c_int,
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    stmt: *mut sqlite3_stmt,
    tail: *const c_char,
    result: c_int,
    work_ready: c_int,
    work_done: c_int,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct ExcPhaseEntry {
    phase: [c_char; EXC_PHASE_MAX_LEN],
    sql: [c_char; EXC_QUERY_MAX_LEN],
    stmt: *mut c_void,
    db: *mut c_void,
    tid: libc::c_ulong,
}

static mut EXC_QUERY_RING: [[c_char; EXC_QUERY_MAX_LEN]; EXC_QUERY_RING_SIZE] =
    [[0; EXC_QUERY_MAX_LEN]; EXC_QUERY_RING_SIZE];
static mut EXC_QUERY_RING_NEXT: c_int = 0;
static mut EXC_QUERY_RING_MUTEX: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;

static mut EXC_PHASE_RING: [ExcPhaseEntry; EXC_PHASE_RING_SIZE] = [ExcPhaseEntry {
    phase: [0; EXC_PHASE_MAX_LEN],
    sql: [0; EXC_QUERY_MAX_LEN],
    stmt: ptr::null_mut(),
    db: ptr::null_mut(),
    tid: 0,
}; EXC_PHASE_RING_SIZE];
static mut EXC_PHASE_RING_NEXT: c_int = 0;
static mut EXC_PHASE_RING_MUTEX: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;

static mut CRASH_LAST_QUERY: [c_char; CRASH_QUERY_MAX_LEN] = [0; CRASH_QUERY_MAX_LEN];
static CRASH_LAST_QUERY_LEN: AtomicI32 = AtomicI32::new(0);
static mut CRASH_LAST_PHASE: [c_char; CRASH_PHASE_MAX_LEN] = [0; CRASH_PHASE_MAX_LEN];
static CRASH_LAST_PHASE_LEN: AtomicI32 = AtomicI32::new(0);

static TRACE_LAST_QUERY_CACHED: AtomicI32 = AtomicI32::new(-1);
static mut TRACE_LAST_QUERY_PATH: *const c_char = ptr::null();

static mut EXCEPTION_TYPES: [ExceptionTypeTracker; MAX_EXCEPTION_TYPES] = [ExceptionTypeTracker {
    type_name: ptr::null(),
    count: 0,
    logged_with_trace: 0,
}; MAX_EXCEPTION_TYPES];
static mut EXCEPTION_TYPE_COUNT: c_int = 0;

static SYMBOLS_VERIFIED: AtomicI32 = AtomicI32::new(0);

#[no_mangle]
pub static mut sqlite_handle: *mut c_void = ptr::null_mut();

#[no_mangle]
pub static mut orig_sqlite3_open: Option<unsafe extern "C" fn(*const c_char, *mut *mut sqlite3) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_open_v2: Option<
    unsafe extern "C" fn(*const c_char, *mut *mut sqlite3, c_int, *const c_char) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_close: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_close_v2: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_exec: Option<
    unsafe extern "C" fn(*mut sqlite3, *const c_char, Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>, *mut c_void, *mut *mut c_char) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_changes: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_changes64: Option<unsafe extern "C" fn(*mut sqlite3) -> i64> = None;
#[no_mangle]
pub static mut orig_sqlite3_last_insert_rowid: Option<unsafe extern "C" fn(*mut sqlite3) -> i64> = None;
#[no_mangle]
pub static mut orig_sqlite3_get_table: Option<
    unsafe extern "C" fn(
        *mut sqlite3,
        *const c_char,
        *mut *mut *mut c_char,
        *mut c_int,
        *mut c_int,
        *mut *mut c_char,
    ) -> c_int,
> = None;

#[no_mangle]
pub static mut orig_sqlite3_errmsg: Option<unsafe extern "C" fn(*mut sqlite3) -> *const c_char> = None;
#[no_mangle]
pub static mut orig_sqlite3_errcode: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_extended_errcode: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int> = None;

#[no_mangle]
pub static mut orig_sqlite3_prepare: Option<
    unsafe extern "C" fn(*mut sqlite3, *const c_char, c_int, *mut *mut sqlite3_stmt, *mut *const c_char) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_prepare_v2: Option<
    unsafe extern "C" fn(*mut sqlite3, *const c_char, c_int, *mut *mut sqlite3_stmt, *mut *const c_char) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_prepare_v3: Option<
    unsafe extern "C" fn(
        *mut sqlite3,
        *const c_char,
        c_int,
        c_uint,
        *mut *mut sqlite3_stmt,
        *mut *const c_char,
    ) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_prepare16_v2: Option<
    unsafe extern "C" fn(*mut sqlite3, *const c_void, c_int, *mut *mut sqlite3_stmt, *mut *const c_void) -> c_int,
> = None;

#[no_mangle]
pub static mut orig_sqlite3_bind_int: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, c_int) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_int64: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, i64) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_double: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, f64) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_text: Option<
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_char, c_int, *mut c_void) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_text64: Option<
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_char, u64, *mut c_void, c_uchar) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_blob: Option<
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_void, c_int, *mut c_void) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_blob64: Option<
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_void, u64, *mut c_void) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_value: Option<
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const crate::ffi_types::sqlite3_value) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_null: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int> = None;

#[no_mangle]
pub static mut orig_sqlite3_step: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_reset: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_finalize: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_clear_bindings: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;

#[no_mangle]
pub static mut orig_sqlite3_column_count: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_type: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_int: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_int64: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> i64> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_double: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> f64> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_text: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_uchar> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_column_blob: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_void> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_column_bytes: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_column_name: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_char> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_column_decltype: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_char> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_column_value: Option<
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *mut crate::ffi_types::sqlite3_value,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_data_count: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;

#[no_mangle]
pub static mut orig_sqlite3_value_type: Option<unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> c_int> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_value_text: Option<
    unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> *const c_uchar,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_value_int: Option<unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> c_int> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_value_int64: Option<unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> i64> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_value_double: Option<unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> f64> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_value_bytes: Option<unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> c_int> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_value_blob: Option<unsafe extern "C" fn(*mut crate::ffi_types::sqlite3_value) -> *const c_void> =
    None;

#[no_mangle]
pub static mut orig_sqlite3_create_collation: Option<
    unsafe extern "C" fn(*mut sqlite3, *const c_char, c_int, *mut c_void, CollationCompare) -> c_int,
> = None;
#[no_mangle]
pub static mut orig_sqlite3_create_collation_v2: Option<
    unsafe extern "C" fn(*mut sqlite3, *const c_char, c_int, *mut c_void, CollationCompare, CollationDestroy) -> c_int,
> = None;

#[no_mangle]
pub static mut orig_sqlite3_free: Option<unsafe extern "C" fn(*mut c_void)> = None;
#[no_mangle]
pub static mut orig_sqlite3_malloc: Option<unsafe extern "C" fn(c_int) -> *mut c_void> = None;
#[no_mangle]
pub static mut orig_sqlite3_db_handle: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut sqlite3> = None;
#[no_mangle]
pub static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char> = None;
#[no_mangle]
pub static mut orig_sqlite3_expanded_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut c_char> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_parameter_count: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_parameter_index: Option<unsafe extern "C" fn(*mut sqlite3_stmt, *const c_char) -> c_int> =
    None;
#[no_mangle]
pub static mut orig_sqlite3_stmt_readonly: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_stmt_busy: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_stmt_status: Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, c_int) -> c_int> = None;
#[no_mangle]
pub static mut orig_sqlite3_bind_parameter_name: Option<
    unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> *const c_char,
> = None;

#[no_mangle]
pub static mut shim_sqlite3_prepare_v2: Option<
    unsafe extern "C" fn(*mut sqlite3, *const c_char, c_int, *mut *mut sqlite3_stmt, *mut *const c_char) -> c_int,
> = None;
#[no_mangle]
pub static mut shim_sqlite3_errmsg: Option<unsafe extern "C" fn(*mut sqlite3) -> *const c_char> = None;
#[no_mangle]
pub static mut shim_sqlite3_errcode: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int> = None;

#[no_mangle]
pub static mut worker_thread: libc::pthread_t = 0 as libc::pthread_t;
#[no_mangle]
pub static mut worker_mutex: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;
#[no_mangle]
pub static mut worker_cond_request: libc::pthread_cond_t = libc::PTHREAD_COND_INITIALIZER;
#[no_mangle]
pub static mut worker_cond_response: libc::pthread_cond_t = libc::PTHREAD_COND_INITIALIZER;
#[no_mangle]
pub static mut worker_request: WorkerRequest = WorkerRequest {
    type_: WORK_NONE,
    db: ptr::null_mut(),
    z_sql: ptr::null(),
    n_byte: 0,
    stmt: ptr::null_mut(),
    tail: ptr::null(),
    result: SQLITE_ERROR,
    work_ready: 0,
    work_done: 0,
};
#[no_mangle]
pub static mut worker_running: c_int = 0;

#[no_mangle]
pub static mut fake_value_pool: [PgFakeValue; MAX_FAKE_VALUES] = [PgFakeValue {
    magic: 0,
    pg_stmt: ptr::null_mut(),
    col_idx: 0,
    row_idx: 0,
    owner_thread: 0 as libc::pthread_t,
}; MAX_FAKE_VALUES];
#[no_mangle]
pub static mut fake_value_next: c_uint = 0;
#[no_mangle]
pub static mut fake_value_mutex: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;

#[no_mangle]
pub static mut shim_initialized: c_int = 0;
#[no_mangle]
pub static mut shim_passthrough_only: c_int = 0;

#[no_mangle]
pub static mut last_query_being_processed: *const c_char = ptr::null();
#[no_mangle]
pub static mut last_column_being_accessed: *const c_char = ptr::null();

#[no_mangle]
pub static mut global_value_type_calls: c_long = 0;
#[no_mangle]
pub static mut global_column_type_calls: c_long = 0;

#[no_mangle]
pub static mut cxa_demangle_fn: Option<
    unsafe extern "C" fn(*const c_char, *mut c_char, *mut libc::size_t, *mut c_int) -> *mut c_char,
> = None;

#[no_mangle]
pub static mut shim_init_pid: libc::pid_t = 0;

#[no_mangle]
pub static total_exception_count: AtomicI32 = AtomicI32::new(0);
#[no_mangle]
pub static mut exception_tracker_mutex: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;

static EXC_LOG_META_ENV: &[u8] = b"PLEX_PG_EXCEPTION_LOG_META\0";
static EXC_DUMP_OBJECT_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_OBJECT\0";
static EXC_DUMP_BYTES_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_BYTES\0";
static EXC_DUMP_POINTERS_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_POINTERS\0";
static EXC_DUMP_TINFO_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_TINFO\0";
static EXC_DUMP_POINTER_MAX_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_POINTERS_MAX\0";
static EXC_DUMP_POINTER_BYTES_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_POINTER_BYTES\0";
static EXC_DUMP_SCAN_STRINGS_ENV: &[u8] = b"PLEX_PG_EXCEPTION_SCAN_STRINGS\0";
static EXC_DUMP_SCAN_STRINGS_BYTES_ENV: &[u8] = b"PLEX_PG_EXCEPTION_SCAN_STRINGS_BYTES\0";

#[cfg(target_os = "macos")]
extern "C" {
    static mut __stderrp: *mut libc::FILE;
}

#[cfg(not(target_os = "macos"))]
extern "C" {
    static mut stderr: *mut libc::FILE;
}

extern "C" {
    fn pg_exception_extract_what(
        thrown_exception: *mut c_void,
        tinfo: *mut c_void,
        out_buf: *mut c_char,
        out_buf_len: libc::size_t,
    ) -> c_int;

    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);

    fn pg_pool_cleanup_after_fork();
    fn pg_logging_reset_after_fork();

    fn pg_config_init();
    fn pg_client_init();
    fn pg_statement_init();
    fn pg_query_cache_init();
    fn sql_translator_init();

    fn pg_statement_cleanup();
    fn pg_client_cleanup();
    fn sql_translator_cleanup();
    fn pg_logging_cleanup();
}

#[inline]
pub(crate) unsafe fn stderr_ptr() -> *mut libc::FILE {
    #[cfg(target_os = "macos")]
    {
        __stderrp
    }
    #[cfg(not(target_os = "macos"))]
    {
        stderr
    }
}

fn env_usize(name: &[u8]) -> Option<usize> {
    env_utils::env_usize(name)
}

#[cfg(target_os = "linux")]
unsafe fn read_process_memory(addr: *const c_void, buf: &mut [u8]) -> isize {
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut c_void,
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: addr as *mut c_void,
        iov_len: buf.len(),
    };
    libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0)
}

#[cfg(not(target_os = "linux"))]
unsafe fn read_process_memory(_addr: *const c_void, _buf: &mut [u8]) -> isize {
    -1
}

fn log_exception_object_dump(thrown_exception: *mut c_void, bytes: usize) -> Vec<usize> {
    if bytes == 0 {
        return Vec::new();
    }
    let max_bytes = bytes.min(1024);
    let mut buf = vec![0u8; max_bytes];
    let n = unsafe { read_process_memory(thrown_exception, &mut buf) };
    if n <= 0 {
        log_info(&format!(
            "EXC_META_DUMP: read failed addr=0x{:x} bytes={}",
            thrown_exception as usize, max_bytes
        ));
        return Vec::new();
    }
    let used = n as usize;
    log_info(&format!(
        "EXC_META_DUMP: addr=0x{:x} bytes={}",
        thrown_exception as usize, used
    ));

    let data = &buf[..used];
    let mut pointers: Vec<usize> = Vec::new();
    let mut ptr_count = 0usize;
    let word = std::mem::size_of::<usize>();
    let aligned_len = data.len().saturating_sub(data.len() % word);
    for offset in (0..aligned_len).step_by(word) {
        let mut raw = [0u8; std::mem::size_of::<usize>()];
        raw.copy_from_slice(&data[offset..offset + word]);
        let val = usize::from_le_bytes(raw);
        if val == 0 {
            continue;
        }
        let looks_canonical = (val >> 48) == 0 || (val >> 48) == 0xffff;
        let aligned = (val & 0x7) == 0;
        if looks_canonical && aligned {
            ptr_count += 1;
            pointers.push(val);
            if ptr_count >= 32 {
                break;
            }
        }
    }
    for (i, chunk) in data.chunks(16).enumerate() {
        let mut hex = String::with_capacity(16 * 3);
        let mut ascii = String::with_capacity(16);
        for &b in chunk {
            hex.push_str(&format!("{:02x} ", b));
            let ch = if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            };
            ascii.push(ch);
        }
        log_info(&format!(
            "EXC_META_DUMP: +0x{:04x} {:<48} |{}|",
            i * 16,
            hex.trim_end(),
            ascii
        ));
    }

    let mut sequences = 0usize;
    let mut start: Option<usize> = None;
    for (idx, &b) in data.iter().enumerate() {
        let printable = (0x20..=0x7e).contains(&b);
        if printable {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(s) = start.take() {
            let len = idx - s;
            if len >= 8 {
                let seq = String::from_utf8_lossy(&data[s..idx]).to_string();
                log_info(&format!("EXC_META_STR: +0x{:04x} len={} '{}'", s, len, seq));
                sequences += 1;
                if sequences >= 8 {
                    log_info("EXC_META_STR: truncated (limit 8)");
                    break;
                }
            }
        }
    }
    if sequences < 8 {
        if let Some(s) = start {
            let len = data.len().saturating_sub(s);
            if len >= 8 {
                let seq = String::from_utf8_lossy(&data[s..]).to_string();
                log_info(&format!("EXC_META_STR: +0x{:04x} len={} '{}'", s, len, seq));
            }
        }
    }
    if !pointers.is_empty() {
        let mut msg = String::from("EXC_META_PTRS:");
        for ptr in &pointers {
            msg.push_str(&format!(" 0x{:x}", ptr));
        }
        log_info(&msg);
    }
    pointers
}

fn log_exception_string_scan(base: *mut c_void, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let max_bytes = bytes.min(4096);
    let mut buf = vec![0u8; max_bytes];
    let n = unsafe { read_process_memory(base, &mut buf) };
    if n <= 0 {
        log_info(&format!(
            "EXC_META_SCAN: read failed addr=0x{:x} bytes={}",
            base as usize, max_bytes
        ));
        return;
    }
    let used = n as usize;
    let data = &buf[..used];
    let mut sequences = 0usize;
    let mut start: Option<usize> = None;
    for (idx, &b) in data.iter().enumerate() {
        let printable = (0x20..=0x7e).contains(&b);
        if printable {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(s) = start.take() {
            let len = idx - s;
            if len >= 12 {
                let seq = String::from_utf8_lossy(&data[s..idx]).to_string();
                log_info(&format!("EXC_META_SCAN: +0x{:04x} len={} '{}'", s, len, seq));
                sequences += 1;
                if sequences >= 12 {
                    log_info("EXC_META_SCAN: truncated (limit 12)");
                    break;
                }
            }
        }
    }
    if sequences < 12 {
        if let Some(s) = start {
            let len = data.len().saturating_sub(s);
            if len >= 12 {
                let seq = String::from_utf8_lossy(&data[s..]).to_string();
                log_info(&format!("EXC_META_SCAN: +0x{:04x} len={} '{}'", s, len, seq));
            }
        }
    }
}

fn write_box_line(left: &[u8], right: &[u8]) {
    unsafe {
        let fd = libc::STDERR_FILENO;
        let _ = libc::write(fd, left.as_ptr() as *const c_void, left.len());
        for _ in 0..BOX_INNER_WIDTH {
            let _ = libc::write(fd, BOX_H.as_ptr() as *const c_void, BOX_H.len());
        }
        let _ = libc::write(fd, right.as_ptr() as *const c_void, right.len());
        let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
    }
}

fn trace_last_query_enabled() -> bool {
    let cached = TRACE_LAST_QUERY_CACHED.load(Ordering::Acquire);
    if cached != -1 {
        return cached != 0;
    }

    let mut enabled = false;
    unsafe {
        let env = libc::getenv(b"PLEX_PG_TRACE_LAST_QUERY\0".as_ptr() as *const c_char);
        if !env.is_null() && *env != 0 && *env != b'0' as c_char {
            enabled = true;
            let path = libc::getenv(b"PLEX_PG_TRACE_LAST_QUERY_FILE\0".as_ptr() as *const c_char);
            if !path.is_null() && *path != 0 {
                TRACE_LAST_QUERY_PATH = path;
            } else {
                TRACE_LAST_QUERY_PATH = TRACE_LAST_QUERY_DEFAULT.as_ptr() as *const c_char;
            }
        }
    }

    TRACE_LAST_QUERY_CACHED.store(if enabled { 1 } else { 0 }, Ordering::Release);
    enabled
}

unsafe extern "C" fn tls_destructor(ptr: *mut c_void) {
    if !ptr.is_null() {
        libc::free(ptr);
    }
}

fn tls_key() -> libc::pthread_key_t {
    TLS_INIT.call_once(|| unsafe {
        let mut key: libc::pthread_key_t = 0;
        if libc::pthread_key_create(&mut key as *mut _, Some(tls_destructor)) == 0 {
            TLS_KEY = key;
        } else {
            TLS_KEY = 0;
        }
    });
    unsafe { TLS_KEY }
}

unsafe fn tls_state() -> *mut TlsState {
    let key = tls_key();
    if key == 0 {
        return ptr::addr_of_mut!(TLS_FALLBACK);
    }
    let ptr_val = libc::pthread_getspecific(key) as *mut TlsState;
    if !ptr_val.is_null() {
        return ptr_val;
    }
    let new = libc::calloc(1, size_of::<TlsState>()) as *mut TlsState;
    if new.is_null() {
        return ptr::addr_of_mut!(TLS_FALLBACK);
    }
    libc::pthread_setspecific(key, new as *mut c_void);
    new
}

pub(crate) fn tls_in_interpose_call_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).in_interpose_call) }
}

pub(crate) fn tls_prepare_v2_depth_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).prepare_v2_depth) }
}

pub(crate) fn tls_in_resolve_tables_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).in_resolve_tables) }
}

pub(crate) fn tls_value_type_calls_ptr() -> *mut c_long {
    unsafe { ptr::addr_of_mut!((*tls_state()).value_type_calls) }
}

pub(crate) fn tls_column_type_calls_ptr() -> *mut c_long {
    unsafe { ptr::addr_of_mut!((*tls_state()).column_type_calls) }
}

pub(crate) fn tls_last_query_ptr() -> *mut *const c_char {
    unsafe { ptr::addr_of_mut!((*tls_state()).last_query) }
}

unsafe fn read_option<T: Copy>(slot: *const Option<T>) -> Option<T> {
    *slot
}

extern "C" fn worker_thread_func(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        log_info(&format!(
            "WORKER: Thread started with {} MB stack",
            WORKER_STACK_SIZE / (1024 * 1024)
        ));

        loop {
            let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));

            while worker_request.work_ready == 0 && worker_running != 0 {
                libc::pthread_cond_wait(
                    ptr::addr_of_mut!(worker_cond_request),
                    worker_guard.mutex_ptr(),
                );
            }

            if worker_running == 0 {
                worker_guard.unlock();
                break;
            }

            worker_request.work_ready = 0;

            if worker_request.type_ == WORK_SHUTDOWN {
                worker_request.work_done = 1;
                libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_response));
                worker_guard.unlock();
                break;
            }

            if worker_request.type_ == WORK_PREPARE_V2 {
                let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
                let mut tail: *const c_char = ptr::null();
                let rc = crate::db_interpose_prepare::rust_my_sqlite3_prepare_v2_internal(
                    worker_request.db,
                    worker_request.z_sql,
                    worker_request.n_byte,
                    &mut stmt,
                    &mut tail,
                    1,
                );

                worker_request.stmt = stmt;
                worker_request.tail = tail;
                worker_request.result = rc;
            }

            worker_request.work_done = 1;
            libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_response));
            worker_guard.unlock();
        }

        log_info("WORKER: Thread exiting");
        ptr::null_mut()
    }
}

unsafe fn get_exception_tracker_impl(type_name: *const c_char) -> *mut ExceptionTypeTracker {
    let mut exc_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(exception_tracker_mutex));

    for i in 0..EXCEPTION_TYPE_COUNT {
        let tracker = &mut EXCEPTION_TYPES[i as usize] as *mut ExceptionTypeTracker;
        let tracker_ref = &mut *tracker;
        if tracker_ref.type_name == type_name
            || (!tracker_ref.type_name.is_null()
                && !type_name.is_null()
                && libc::strcmp(tracker_ref.type_name, type_name) == 0)
        {
            tracker_ref.count += 1;
            exc_guard.unlock();
            return tracker;
        }
    }

    if (EXCEPTION_TYPE_COUNT as usize) < MAX_EXCEPTION_TYPES {
        let tracker = &mut EXCEPTION_TYPES[EXCEPTION_TYPE_COUNT as usize] as *mut ExceptionTypeTracker;
        (*tracker).type_name = type_name;
        (*tracker).count = 1;
        (*tracker).logged_with_trace = 0;
        EXCEPTION_TYPE_COUNT += 1;
        exc_guard.unlock();
        return tracker;
    }

    exc_guard.unlock();
    ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn rust_get_exception_tracker(type_name: *const c_char) -> *mut ExceptionTypeTracker {
    unsafe { get_exception_tracker_impl(type_name) }
}

#[no_mangle]
pub extern "C" fn rust_reset_exception_tracking() {
    total_exception_count.store(0, Ordering::SeqCst);
    unsafe {
        EXCEPTION_TYPE_COUNT = 0;
    }
}

#[no_mangle]
pub extern "C" fn rust_get_type_name(tinfo: *mut c_void) -> *const c_char {
    if tinfo.is_null() {
        return UNKNOWN_STR.as_ptr() as *const c_char;
    }
    unsafe {
        let name_ptr = (tinfo as *const *const c_char).add(1);
        let name = *name_ptr;
        if name.is_null() {
            UNKNOWN_STR.as_ptr() as *const c_char
        } else {
            name
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_pg_check_fake_value(p_val: *mut crate::ffi_types::sqlite3_value) -> *mut PgFakeValue {
    if p_val.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        let ptr_val = p_val as usize;
        let pool_ptr = ptr::addr_of!(fake_value_pool) as *const PgFakeValue;
        let pool_start = pool_ptr as usize;
        let pool_end = pool_ptr.add(MAX_FAKE_VALUES) as usize;
        if ptr_val >= pool_start && ptr_val < pool_end {
            let fake = p_val as *mut PgFakeValue;
            if (*fake).magic == PG_FAKE_VALUE_MAGIC {
                return fake;
            }
        }
    }
    ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn rust_rewrite_blobs_schema_migrations(_sql: *const c_char, _db_path: *const c_char) -> *mut c_char {
    ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn rust_simple_str_replace(str_ptr: *const c_char, old_ptr: *const c_char, new_ptr: *const c_char) -> *mut c_char {
    if str_ptr.is_null() || old_ptr.is_null() || new_ptr.is_null() {
        return ptr::null_mut();
    }

    unsafe {
        let pos = libc::strstr(str_ptr, old_ptr);
        if pos.is_null() {
            return ptr::null_mut();
        }

        let old_len = libc::strlen(old_ptr);
        let new_len = libc::strlen(new_ptr);
        let str_len = libc::strlen(str_ptr);
        let result_len = str_len - old_len + new_len;

        let result = libc::malloc(result_len + 1) as *mut c_char;
        if result.is_null() {
            return ptr::null_mut();
        }

        let prefix_len = (pos as usize).wrapping_sub(str_ptr as usize);
        libc::memcpy(result as *mut c_void, str_ptr as *const c_void, prefix_len);
        libc::memcpy(
            result.add(prefix_len) as *mut c_void,
            new_ptr as *const c_void,
            new_len,
        );
        libc::strcpy(result.add(prefix_len + new_len), pos.add(old_len));

        result
    }
}

#[no_mangle]
pub extern "C" fn rust_common_load_sqlite_symbols(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }

    unsafe {
        load_sym!(orig_sqlite3_open, handle, b"sqlite3_open\0");
        load_sym!(orig_sqlite3_open_v2, handle, b"sqlite3_open_v2\0");
        load_sym!(orig_sqlite3_close, handle, b"sqlite3_close\0");
        load_sym!(orig_sqlite3_close_v2, handle, b"sqlite3_close_v2\0");

        load_sym!(orig_sqlite3_exec, handle, b"sqlite3_exec\0");
        load_sym!(orig_sqlite3_get_table, handle, b"sqlite3_get_table\0");

        load_sym!(orig_sqlite3_changes, handle, b"sqlite3_changes\0");
        load_sym!(orig_sqlite3_changes64, handle, b"sqlite3_changes64\0");
        load_sym!(orig_sqlite3_last_insert_rowid, handle, b"sqlite3_last_insert_rowid\0");

        load_sym!(orig_sqlite3_errmsg, handle, b"sqlite3_errmsg\0");
        load_sym!(orig_sqlite3_errcode, handle, b"sqlite3_errcode\0");
        load_sym!(orig_sqlite3_extended_errcode, handle, b"sqlite3_extended_errcode\0");

        load_sym!(orig_sqlite3_prepare, handle, b"sqlite3_prepare\0");
        load_sym!(orig_sqlite3_prepare_v2, handle, b"sqlite3_prepare_v2\0");
        load_sym!(orig_sqlite3_prepare_v3, handle, b"sqlite3_prepare_v3\0");
        load_sym!(orig_sqlite3_prepare16_v2, handle, b"sqlite3_prepare16_v2\0");

        load_sym!(orig_sqlite3_bind_int, handle, b"sqlite3_bind_int\0");
        load_sym!(orig_sqlite3_bind_int64, handle, b"sqlite3_bind_int64\0");
        load_sym!(orig_sqlite3_bind_double, handle, b"sqlite3_bind_double\0");
        load_sym!(orig_sqlite3_bind_text, handle, b"sqlite3_bind_text\0");
        load_sym!(orig_sqlite3_bind_text64, handle, b"sqlite3_bind_text64\0");
        load_sym!(orig_sqlite3_bind_blob, handle, b"sqlite3_bind_blob\0");
        load_sym!(orig_sqlite3_bind_blob64, handle, b"sqlite3_bind_blob64\0");
        load_sym!(orig_sqlite3_bind_value, handle, b"sqlite3_bind_value\0");
        load_sym!(orig_sqlite3_bind_null, handle, b"sqlite3_bind_null\0");

        load_sym!(orig_sqlite3_step, handle, b"sqlite3_step\0");
        load_sym!(orig_sqlite3_reset, handle, b"sqlite3_reset\0");
        load_sym!(orig_sqlite3_finalize, handle, b"sqlite3_finalize\0");
        load_sym!(orig_sqlite3_clear_bindings, handle, b"sqlite3_clear_bindings\0");

        load_sym!(orig_sqlite3_column_count, handle, b"sqlite3_column_count\0");
        load_sym!(orig_sqlite3_column_type, handle, b"sqlite3_column_type\0");
        load_sym!(orig_sqlite3_column_int, handle, b"sqlite3_column_int\0");
        load_sym!(orig_sqlite3_column_int64, handle, b"sqlite3_column_int64\0");
        load_sym!(orig_sqlite3_column_double, handle, b"sqlite3_column_double\0");
        load_sym!(orig_sqlite3_column_text, handle, b"sqlite3_column_text\0");
        load_sym!(orig_sqlite3_column_blob, handle, b"sqlite3_column_blob\0");
        load_sym!(orig_sqlite3_column_bytes, handle, b"sqlite3_column_bytes\0");
        load_sym!(orig_sqlite3_column_name, handle, b"sqlite3_column_name\0");
        load_sym!(orig_sqlite3_column_decltype, handle, b"sqlite3_column_decltype\0");
        load_sym!(orig_sqlite3_column_value, handle, b"sqlite3_column_value\0");
        load_sym!(orig_sqlite3_data_count, handle, b"sqlite3_data_count\0");

        load_sym!(orig_sqlite3_value_type, handle, b"sqlite3_value_type\0");
        load_sym!(orig_sqlite3_value_text, handle, b"sqlite3_value_text\0");
        load_sym!(orig_sqlite3_value_int, handle, b"sqlite3_value_int\0");
        load_sym!(orig_sqlite3_value_int64, handle, b"sqlite3_value_int64\0");
        load_sym!(orig_sqlite3_value_double, handle, b"sqlite3_value_double\0");
        load_sym!(orig_sqlite3_value_bytes, handle, b"sqlite3_value_bytes\0");
        load_sym!(orig_sqlite3_value_blob, handle, b"sqlite3_value_blob\0");

        load_sym!(orig_sqlite3_create_collation, handle, b"sqlite3_create_collation\0");
        load_sym!(orig_sqlite3_create_collation_v2, handle, b"sqlite3_create_collation_v2\0");

        load_sym!(orig_sqlite3_free, handle, b"sqlite3_free\0");
        load_sym!(orig_sqlite3_malloc, handle, b"sqlite3_malloc\0");
        load_sym!(orig_sqlite3_db_handle, handle, b"sqlite3_db_handle\0");
        load_sym!(orig_sqlite3_sql, handle, b"sqlite3_sql\0");
        load_sym!(orig_sqlite3_expanded_sql, handle, b"sqlite3_expanded_sql\0");
        load_sym!(orig_sqlite3_bind_parameter_count, handle, b"sqlite3_bind_parameter_count\0");
        load_sym!(orig_sqlite3_bind_parameter_index, handle, b"sqlite3_bind_parameter_index\0");
        load_sym!(orig_sqlite3_bind_parameter_name, handle, b"sqlite3_bind_parameter_name\0");
        load_sym!(orig_sqlite3_stmt_readonly, handle, b"sqlite3_stmt_readonly\0");
        load_sym!(orig_sqlite3_stmt_busy, handle, b"sqlite3_stmt_busy\0");
        load_sym!(orig_sqlite3_stmt_status, handle, b"sqlite3_stmt_status\0");

        if read_option(ptr::addr_of!(shim_sqlite3_prepare_v2)).is_none() {
            *ptr::addr_of_mut!(shim_sqlite3_prepare_v2) = read_option(ptr::addr_of!(orig_sqlite3_prepare_v2));
        }
        if read_option(ptr::addr_of!(shim_sqlite3_errmsg)).is_none() {
            *ptr::addr_of_mut!(shim_sqlite3_errmsg) = read_option(ptr::addr_of!(orig_sqlite3_errmsg));
        }
        if read_option(ptr::addr_of!(shim_sqlite3_errcode)).is_none() {
            *ptr::addr_of_mut!(shim_sqlite3_errcode) = read_option(ptr::addr_of!(orig_sqlite3_errcode));
        }

        let open_fn = read_option(ptr::addr_of!(orig_sqlite3_open));
        if let Some(f) = open_fn {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] orig_sqlite3_open = %p\n\0".as_ptr() as *const c_char,
                f as *const c_void,
            );
        } else {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] WARNING: orig_sqlite3_open is NULL!\n\0".as_ptr() as *const c_char,
            );
        }
        let prep_fn = read_option(ptr::addr_of!(orig_sqlite3_prepare_v2));
        if let Some(f) = prep_fn {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] orig_sqlite3_prepare_v2 = %p\n\0".as_ptr() as *const c_char,
                f as *const c_void,
            );
        } else {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] WARNING: orig_sqlite3_prepare_v2 is NULL!\n\0".as_ptr() as *const c_char,
            );
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_shim_ensure_ready() -> c_int {
    if SYMBOLS_VERIFIED.load(Ordering::Acquire) != 0 {
        return 1;
    }

    std::sync::atomic::fence(Ordering::SeqCst);

    unsafe {
        if shim_initialized == 0 {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM] WARNING: shim_ensure_ready called before shim_initialized!\n\0".as_ptr() as *const c_char,
            );
            libc::fflush(stderr_ptr());
            return 0;
        }

        let open_missing = read_option(ptr::addr_of!(orig_sqlite3_open)).is_none();
        let prep_missing = read_option(ptr::addr_of!(orig_sqlite3_prepare_v2)).is_none();
        let step_missing = read_option(ptr::addr_of!(orig_sqlite3_step)).is_none();
        if open_missing || prep_missing || step_missing {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM] WARNING: Critical symbols NULL, attempting fallback...\n\0".as_ptr() as *const c_char,
            );
            libc::fflush(stderr_ptr());

            if cfg!(target_os = "macos") {
                if !sqlite_handle.is_null() {
                    load_sym!(orig_sqlite3_open, sqlite_handle, b"sqlite3_open\0");
                    load_sym!(orig_sqlite3_prepare_v2, sqlite_handle, b"sqlite3_prepare_v2\0");
                    load_sym!(orig_sqlite3_step, sqlite_handle, b"sqlite3_step\0");
                }
            } else {
                load_sym!(orig_sqlite3_open, libc::RTLD_NEXT, b"sqlite3_open\0");
                load_sym!(orig_sqlite3_prepare_v2, libc::RTLD_NEXT, b"sqlite3_prepare_v2\0");
                load_sym!(orig_sqlite3_step, libc::RTLD_NEXT, b"sqlite3_step\0");
            }

            let open_missing = read_option(ptr::addr_of!(orig_sqlite3_open)).is_none();
            let prep_missing = read_option(ptr::addr_of!(orig_sqlite3_prepare_v2)).is_none();
            let step_missing = read_option(ptr::addr_of!(orig_sqlite3_step)).is_none();
            if open_missing || prep_missing || step_missing {
                libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM] FATAL: Cannot resolve critical SQLite symbols!\n\0".as_ptr() as *const c_char,
                );
                libc::fflush(stderr_ptr());
                return 0;
            }
        }
    }

    SYMBOLS_VERIFIED.store(1, Ordering::Release);
    1
}

#[no_mangle]
pub extern "C" fn rust_reset_symbol_verification() {
    SYMBOLS_VERIFIED.store(0, Ordering::SeqCst);
}

#[no_mangle]
pub extern "C" fn rust_worker_init() -> c_int {
    unsafe {
        let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
        if libc::pthread_attr_init(attr.as_mut_ptr()) != 0 {
            log_error("WORKER: Failed to init thread attributes");
            return -1;
        }
        let mut attr = attr.assume_init();

        if libc::pthread_attr_setstacksize(&mut attr as *mut _, WORKER_STACK_SIZE) != 0 {
            log_error("WORKER: Failed to set stack size");
            libc::pthread_attr_destroy(&mut attr as *mut _);
            return -1;
        }

        worker_running = 1;
        worker_request = WorkerRequest {
            type_: WORK_NONE,
            db: ptr::null_mut(),
            z_sql: ptr::null(),
            n_byte: 0,
            stmt: ptr::null_mut(),
            tail: ptr::null(),
            result: SQLITE_ERROR,
            work_ready: 0,
            work_done: 0,
        };

        if libc::pthread_create(
            ptr::addr_of_mut!(worker_thread),
            &attr as *const _,
            worker_thread_func,
            ptr::null_mut(),
        ) != 0
        {
            log_error("WORKER: Failed to create thread");
            worker_running = 0;
            libc::pthread_attr_destroy(&mut attr as *mut _);
            return -1;
        }

        libc::pthread_attr_destroy(&mut attr as *mut _);
        log_info(&format!(
            "WORKER: Initialized with {} MB stack",
            WORKER_STACK_SIZE / (1024 * 1024)
        ));
    }

    0
}

#[no_mangle]
pub extern "C" fn rust_worker_cleanup() {
    unsafe {
        if worker_running == 0 {
            return;
        }

        let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));
        worker_request.type_ = WORK_SHUTDOWN;
        worker_request.work_ready = 1;
        worker_running = 0;
        libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_request));
        worker_guard.unlock();

        libc::pthread_join(worker_thread, ptr::null_mut());
    }

    log_info("WORKER: Cleaned up");
}

#[no_mangle]
pub extern "C" fn rust_delegate_prepare_to_worker(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    unsafe {
        if worker_running == 0 {
            log_error("WORKER: Not running, cannot delegate");
            return SQLITE_ERROR;
        }

        let preview = if z_sql.is_null() {
            "NULL".to_string()
        } else {
            let bytes = CStr::from_ptr(z_sql).to_bytes();
            let slice = &bytes[..bytes.len().min(100)];
            String::from_utf8_lossy(slice).into_owned()
        };
        log_debug(&format!("WORKER: Delegating query ({})", preview));

        let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));

        worker_request.type_ = WORK_PREPARE_V2;
        worker_request.db = db;
        worker_request.z_sql = z_sql;
        worker_request.n_byte = n_byte;
        worker_request.stmt = ptr::null_mut();
        worker_request.tail = ptr::null();
        worker_request.result = SQLITE_ERROR;
        worker_request.work_done = 0;
        worker_request.work_ready = 1;

        libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_request));

        while worker_request.work_done == 0 {
            libc::pthread_cond_wait(
                ptr::addr_of_mut!(worker_cond_response),
                worker_guard.mutex_ptr(),
            );
        }

        if !pp_stmt.is_null() {
            *pp_stmt = worker_request.stmt;
        }
        if !pz_tail.is_null() {
            *pz_tail = worker_request.tail;
        }
        let result = worker_request.result;

        worker_guard.unlock();

        log_debug(&format!("WORKER: Delegation complete, rc={}", result));
        result
    }
}

#[no_mangle]
pub extern "C" fn rust_common_atfork_prepare() {}

#[no_mangle]
pub extern "C" fn rust_common_atfork_parent() {}

#[no_mangle]
pub extern "C" fn rust_common_atfork_child() {
    unsafe {
        libc::fprintf(
            stderr_ptr(),
            b"[FORK_CHILD] Cleaning up inherited connection pool (child PID %d)\n\0".as_ptr() as *const c_char,
            libc::getpid(),
        );
        libc::fflush(stderr_ptr());

        last_query_being_processed = ptr::null();
        last_column_being_accessed = ptr::null();
        global_value_type_calls = 0;
        global_column_type_calls = 0;

        rust_reset_exception_tracking();
        rust_reset_symbol_verification();

        pg_pool_cleanup_after_fork();
        pg_logging_reset_after_fork();

        libc::fprintf(
            stderr_ptr(),
            b"[FORK_CHILD] Pool and logging reset, child will reinitialize\n\0".as_ptr() as *const c_char,
        );
        libc::fflush(stderr_ptr());
    }
}

#[no_mangle]
pub extern "C" fn rust_common_check_fork() -> c_int {
    let current_pid = unsafe { libc::getpid() };
    unsafe {
        if shim_init_pid != 0 && shim_init_pid != current_pid {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Detected fork (parent PID %d, our PID %d) - resetting state\n\0".as_ptr()
                    as *const c_char,
                shim_init_pid,
                current_pid,
            );
            libc::fflush(stderr_ptr());

            shim_initialized = 0;
            last_query_being_processed = ptr::null();
            last_column_being_accessed = ptr::null();
            global_value_type_calls = 0;
            global_column_type_calls = 0;
            rust_reset_exception_tracking();

            shim_init_pid = current_pid;
            return 1;
        }

        shim_init_pid = current_pid;
    }
    0
}

#[no_mangle]
pub extern "C" fn rust_common_shim_init_modules() {
    unsafe {
        pg_config_init();
        pg_client_init();
        pg_statement_init();
        pg_query_cache_init();
        sql_translator_init();
    }
    rust_worker_init();
}

#[no_mangle]
pub extern "C" fn rust_common_shim_cleanup() {
    rust_worker_cleanup();
    unsafe {
        pg_statement_cleanup();
        pg_client_cleanup();
        sql_translator_cleanup();
        pg_logging_cleanup();
    }
}

#[no_mangle]
pub extern "C" fn rust_common_signal_handler(sig: c_int) {
    let (sig_name, sig_desc) = match sig {
        libc::SIGSEGV => (
            b"SIGSEGV\0".as_ptr() as *const c_char,
            b"Segmentation fault\0".as_ptr() as *const c_char,
        ),
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        libc::SIGBUS => (
            b"SIGBUS\0".as_ptr() as *const c_char,
            b"Bus error\0".as_ptr() as *const c_char,
        ),
        libc::SIGFPE => (b"SIGFPE\0".as_ptr() as *const c_char, b"Floating point exception\0".as_ptr() as *const c_char),
        libc::SIGILL => (b"SIGILL\0".as_ptr() as *const c_char, b"Illegal instruction\0".as_ptr() as *const c_char),
        libc::SIGABRT => (b"SIGABRT\0".as_ptr() as *const c_char, b"Abort\0".as_ptr() as *const c_char),
        _ => (b"UNKNOWN\0".as_ptr() as *const c_char, b"Unknown signal\0".as_ptr() as *const c_char),
    };

    unsafe {
        let fd = libc::STDERR_FILENO;
        let _ = libc::write(fd, b"\n[SHIM_FATAL] ".as_ptr() as *const c_void, 14);
        let name_cstr = CStr::from_ptr(sig_name);
        let _ = libc::write(fd, name_cstr.as_ptr() as *const c_void, name_cstr.to_bytes().len());
        let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);

        let plen = CRASH_LAST_PHASE_LEN.load(Ordering::SeqCst);
        if plen > 0 && (plen as usize) < CRASH_PHASE_MAX_LEN {
            let _ = libc::write(fd, b"Last Phase: ".as_ptr() as *const c_void, 12);
            let _ = libc::write(fd, ptr::addr_of!(CRASH_LAST_PHASE) as *const c_void, plen as usize);
            let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
        }

        let qlen = CRASH_LAST_QUERY_LEN.load(Ordering::SeqCst);
        if qlen > 0 && (qlen as usize) < CRASH_QUERY_MAX_LEN {
            let _ = libc::write(fd, b"Last Query: ".as_ptr() as *const c_void, 12);
            let _ = libc::write(fd, ptr::addr_of!(CRASH_LAST_QUERY) as *const c_void, qlen as usize);
            let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
        }
    }

    unsafe {
        libc::fprintf(stderr_ptr(), b"\n\0".as_ptr() as *const c_char);
        write_box_line(BOX_TL, BOX_TR);
        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 FATAL SIGNAL: %-64s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
            sig_name,
        );
        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 Description:  %-64s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
            sig_desc,
        );
        write_box_line(BOX_ML, BOX_MR);

        let ctx_query = last_query_being_processed;
        let ctx_column = last_column_being_accessed;
        if !ctx_query.is_null() {
            let mut q: [c_char; 65] = [0; 65];
            libc::snprintf(q.as_mut_ptr(), q.len(), b"%.64s\0".as_ptr() as *const c_char, ctx_query);
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 Last Query:  %-65s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                q.as_ptr(),
            );
        }
        if !ctx_column.is_null() {
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 Last Column: %-65s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                ctx_column,
            );
        }

        write_box_line(BOX_BL, BOX_BR);
        platform_print_backtrace(sig_name, 1);
    }

    log_error(&format!(
        "FATAL SIGNAL: {} ({})",
        unsafe { CStr::from_ptr(sig_name).to_string_lossy() },
        unsafe { CStr::from_ptr(sig_desc).to_string_lossy() }
    ));

    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

#[no_mangle]
pub extern "C" fn rust_print_exception_info(
    type_name: *const c_char,
    count: c_int,
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
) -> *mut c_char {
    unsafe {
        if read_option(ptr::addr_of!(cxa_demangle_fn)).is_none() {
            let sym = libc::dlsym(libc::RTLD_DEFAULT, b"__cxa_demangle\0".as_ptr() as *const c_char);
            if !sym.is_null() {
                *ptr::addr_of_mut!(cxa_demangle_fn) = Some(std::mem::transmute(sym));
            }
        }

        let mut demangled: *mut c_char = ptr::null_mut();
        if let Some(demangle) = read_option(ptr::addr_of!(cxa_demangle_fn)) {
            if !type_name.is_null() {
                let mut status: c_int = 0;
                demangled = demangle(type_name, ptr::null_mut(), ptr::null_mut(), &mut status);
            }
        }
        let readable_name = if !demangled.is_null() { demangled } else { type_name };

        let ctx_query = last_query_being_processed;
        let ctx_column = last_column_being_accessed;
        let ctx_value_calls = global_value_type_calls;
        let ctx_column_calls = global_column_type_calls;
        let tls_column_type_calls = *tls_column_type_calls_ptr();
        let tls_value_type_calls = *tls_value_type_calls_ptr();
        let tls_last_query = *tls_last_query_ptr();
        let is_shim_related = ctx_value_calls > 0 || ctx_column_calls > 0 || !ctx_query.is_null();
        let tls_is_shim_related =
            tls_column_type_calls > 0 || tls_value_type_calls > 0 || !tls_last_query.is_null();

        let tid = libc::pthread_self();

        libc::fprintf(stderr_ptr(), b"\n\0".as_ptr() as *const c_char);
        write_box_line(BOX_TL, BOX_TR);
        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 C++ EXCEPTION #%-4d                                                          \xE2\x95\x91\n\0"
                .as_ptr() as *const c_char,
            count,
        );
        write_box_line(BOX_ML, BOX_MR);

        let mut type_display: [c_char; 73] = [0; 73];
        if !readable_name.is_null() {
            libc::snprintf(
                type_display.as_mut_ptr(),
                type_display.len(),
                b"%.72s\0".as_ptr() as *const c_char,
                readable_name,
            );
        }
        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 Type: %-72s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
            type_display.as_ptr(),
        );

        let mut what_buf: [c_char; 193] = [0; 193];
        let has_what = pg_exception_extract_what(
            thrown_exception,
            tinfo,
            what_buf.as_mut_ptr(),
            what_buf.len(),
        );
        if has_what != 0 {
            let mut what_display: [c_char; 73] = [0; 73];
            libc::snprintf(
                what_display.as_mut_ptr(),
                what_display.len(),
                b"%.72s\0".as_ptr() as *const c_char,
                what_buf.as_ptr(),
            );
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 What: %-72s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                what_display.as_ptr(),
            );
        } else {
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 What: %-72s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                b"(unavailable at throw site)\0".as_ptr() as *const c_char,
            );
        }

        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 PID: %-6d  Thread: 0x%-54lx \xE2\x95\x91\n\0".as_ptr() as *const c_char,
            libc::getpid(),
            tid as libc::c_ulong,
        );

        write_box_line(BOX_ML, BOX_MR);

        if is_shim_related {
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 SHIM STATE:                                                                  \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
            );
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91   Global: col_type=%-5ld val_type=%-5ld                                      \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
                ctx_column_calls,
                ctx_value_calls,
            );
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91   Thread: col_type=%-5ld val_type=%-5ld (this_thread_used_shim=%s)           \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
                tls_column_type_calls,
                tls_value_type_calls,
                if tls_is_shim_related {
                    b"YES\0".as_ptr() as *const c_char
                } else {
                    b"NO \0".as_ptr() as *const c_char
                },
            );
            if !tls_is_shim_related {
                libc::fprintf(
                    stderr_ptr(),
                    b"\xE2\x95\x91   NOTE: This thread has NOT made any SQLite calls through shim!             \xE2\x95\x91\n\0"
                        .as_ptr() as *const c_char,
                );
            }
            if !ctx_query.is_null() && *ctx_query != 0 {
                let mut query_snippet: [c_char; 55] = [0; 55];
                libc::snprintf(
                    query_snippet.as_mut_ptr(),
                    query_snippet.len(),
                    b"%.54s\0".as_ptr() as *const c_char,
                    ctx_query,
                );
                libc::fprintf(
                    stderr_ptr(),
                    b"\xE2\x95\x91   Last Query (any thread): %-51s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                    query_snippet.as_ptr(),
                );
            }
            if !ctx_column.is_null() && *ctx_column != 0 {
                libc::fprintf(
                    stderr_ptr(),
                    b"\xE2\x95\x91   Last Column: %-63s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                    ctx_column,
                );
            }
        } else {
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 NOT SHIM-RELATED: No SQLite calls have been made through the shim            \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
            );
        }

        log_error(&format!(
            "EXCEPTION #{} [{}]: what='{}' shim={} tls_shim={} col={} val={}",
            count,
            if !readable_name.is_null() {
                CStr::from_ptr(readable_name).to_string_lossy()
            } else {
                "".into()
            },
            if has_what != 0 {
                CStr::from_ptr(what_buf.as_ptr()).to_string_lossy()
            } else {
                "".into()
            },
            if is_shim_related { "YES" } else { "NO" },
            if tls_is_shim_related { "YES" } else { "NO" },
            ctx_column_calls,
            ctx_value_calls
        ));

        demangled
    }
}

#[no_mangle]
pub extern "C" fn rust_common_handle_exception(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
    in_handler_flag: *mut c_int,
    should_call_original: *mut c_int,
) -> c_int {
    if in_handler_flag.is_null() || should_call_original.is_null() {
        return 0;
    }

    unsafe {
        *should_call_original = 1;
        if *in_handler_flag != 0 {
            return 0;
        }
        *in_handler_flag = 1;
    }

    let total_count = total_exception_count.fetch_add(1, Ordering::SeqCst) + 1;

    if thrown_exception.is_null() || tinfo.is_null() {
        unsafe {
            *in_handler_flag = 0;
        }
        return 0;
    }

    let type_name = rust_get_type_name(tinfo);
    let tracker = unsafe { get_exception_tracker_impl(type_name) };

    let should_log_meta = env_utils::env_truthy(EXC_LOG_META_ENV);
    let should_dump_object = env_utils::env_truthy(EXC_DUMP_OBJECT_ENV);

    if should_log_meta {
        let type_addr = tinfo as usize;
        let throw_addr = thrown_exception as usize;
        let pid = unsafe { libc::getpid() };
        let tid = unsafe { libc::pthread_self() };
        log_info(&format!(
            "EXC_META: pid={} tid=0x{:x} thrown=0x{:x} tinfo=0x{:x} total={}",
            pid, tid as usize, throw_addr, type_addr, total_count
        ));
        if !type_name.is_null() {
            let raw = unsafe { CStr::from_ptr(type_name).to_string_lossy() };
            log_info(&format!("EXC_META: type_name_raw={}", raw));
        }
    }
    if should_dump_object {
        let bytes = env_usize(EXC_DUMP_BYTES_ENV).unwrap_or(256);
        let pointers = log_exception_object_dump(thrown_exception, bytes);
        let dump_pointers = env_utils::env_truthy(EXC_DUMP_POINTERS_ENV);
        if dump_pointers {
            let max_ptrs = env_usize(EXC_DUMP_POINTER_MAX_ENV).unwrap_or(6);
            let ptr_bytes = env_usize(EXC_DUMP_POINTER_BYTES_ENV).unwrap_or(512);
            for (idx, ptr) in pointers.into_iter().enumerate() {
                if idx >= max_ptrs {
                    log_info("EXC_META_PTR_DUMP: truncated");
                    break;
                }
                log_info(&format!(
                    "EXC_META_PTR_DUMP: addr=0x{:x} bytes={}",
                    ptr, ptr_bytes
                ));
                let _ = log_exception_object_dump(ptr as *mut c_void, ptr_bytes);
            }
        }
        let dump_tinfo = env_utils::env_truthy(EXC_DUMP_TINFO_ENV);
        if dump_tinfo {
            log_info(&format!("EXC_META_TINFO_DUMP: addr=0x{:x} bytes=256", tinfo as usize));
            let _ = log_exception_object_dump(tinfo as *mut c_void, 256);
        }
        if env_utils::env_truthy(EXC_DUMP_SCAN_STRINGS_ENV) {
            let scan_bytes = env_usize(EXC_DUMP_SCAN_STRINGS_BYTES_ENV).unwrap_or(2048);
            log_info(&format!(
                "EXC_META_SCAN: addr=0x{:x} bytes={}",
                thrown_exception as usize, scan_bytes
            ));
            log_exception_string_scan(thrown_exception, scan_bytes);
        }
    }

    unsafe {
        let verbose_env = libc::getenv(b"PLEX_PG_EXCEPTION_VERBOSE\0".as_ptr() as *const c_char);
        let verbose_exceptions = !verbose_env.is_null() && libc::strcmp(verbose_env, b"0\0".as_ptr() as *const c_char) != 0;
        let nonshim_env = libc::getenv(b"PLEX_PG_EXCEPTION_LOG_NONSHIM_DB\0".as_ptr() as *const c_char);
        let log_nonshim_db = !nonshim_env.is_null() && libc::strcmp(nonshim_env, b"0\0".as_ptr() as *const c_char) != 0;

        let mut is_db_exception = false;
        if !type_name.is_null() {
            let n2db = libc::strstr(type_name, b"N2DB\0".as_ptr() as *const c_char);
            let db9 = libc::strstr(type_name, b"DB9Exception\0".as_ptr() as *const c_char);
            let dbxx = libc::strstr(type_name, b"DB::Exception\0".as_ptr() as *const c_char);
            is_db_exception = !n2db.is_null() || !db9.is_null() || !dbxx.is_null();
        }

        let tls_column_type_calls = *tls_column_type_calls_ptr();
        let tls_value_type_calls = *tls_value_type_calls_ptr();
        let tls_last_query = *tls_last_query_ptr();
        let this_thread_used_shim =
            tls_column_type_calls > 0 || tls_value_type_calls > 0 || !tls_last_query.is_null();

        let should_log = verbose_exceptions
            || (is_db_exception && (this_thread_used_shim || log_nonshim_db))
            || ((total_count as c_int) <= MAX_LOGGED_TOTAL
                && (tracker.is_null() || (*tracker).count <= MAX_LOGGED_PER_TYPE)
                && this_thread_used_shim);

        let should_trace = is_db_exception || (!tracker.is_null() && (*tracker).logged_with_trace == 0);

        if should_log {
            let demangled = rust_print_exception_info(type_name, total_count, thrown_exception, tinfo);

            if should_trace {
                if !tracker.is_null() {
                    (*tracker).logged_with_trace = 1;
                }
                if is_db_exception || verbose_exceptions {
                    rust_pg_exception_dump_recent_queries();
                    rust_pg_exception_dump_recent_phases();
                }
                platform_print_backtrace(b"Exception Stack Trace\0".as_ptr() as *const c_char, 2);
            }

            write_box_line(BOX_BL, BOX_BR);
            libc::fflush(stderr_ptr());

            if !demangled.is_null() {
                libc::free(demangled as *mut c_void);
            }
        } else if (total_count as c_int) == MAX_LOGGED_TOTAL + 1 {
            libc::fprintf(stderr_ptr(), b"\n\0".as_ptr() as *const c_char);
            write_box_line(BOX_TL, BOX_TR);
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 [THROTTLE] Exception logging limited (>%d). Summary in log file.              \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
                MAX_LOGGED_TOTAL,
            );
            write_box_line(BOX_BL, BOX_BR);
            libc::fflush(stderr_ptr());
        }

        *in_handler_flag = 0;
    }

    1
}

#[no_mangle]
pub extern "C" fn rust_pg_exception_get_last_query() -> *const c_char {
    unsafe { last_query_being_processed }
}

#[no_mangle]
pub extern "C" fn rust_pg_exception_get_last_column() -> *const c_char {
    unsafe { last_column_being_accessed }
}

#[no_mangle]
pub extern "C" fn rust_pg_exception_note_query(sql: *const c_char) {
    if sql.is_null() {
        return;
    }
    unsafe {
        if *sql == 0 {
            return;
        }
        let mut ring_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_QUERY_RING_MUTEX));
        libc::snprintf(
            EXC_QUERY_RING[EXC_QUERY_RING_NEXT as usize].as_mut_ptr(),
            EXC_QUERY_MAX_LEN,
            b"%.319s\0".as_ptr() as *const c_char,
            sql,
        );
        EXC_QUERY_RING_NEXT = (EXC_QUERY_RING_NEXT + 1) % (EXC_QUERY_RING_SIZE as c_int);
        ring_guard.unlock();
    }
}

#[no_mangle]
pub extern "C" fn rust_pg_exception_dump_recent_queries() {
    unsafe {
        let mut ring_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_QUERY_RING_MUTEX));
        libc::fprintf(
            stderr_ptr(),
            b"[EXC_CONTEXT] Recent SQL (oldest -> newest):\n\0".as_ptr() as *const c_char,
        );
        for i in 0..EXC_QUERY_RING_SIZE {
            let idx = (EXC_QUERY_RING_NEXT + i as c_int) % (EXC_QUERY_RING_SIZE as c_int);
            let entry = EXC_QUERY_RING[idx as usize];
            if entry[0] != 0 {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] %.319s\n\0".as_ptr() as *const c_char,
                    i as c_int,
                    entry.as_ptr(),
                );
            }
        }
        libc::fflush(stderr_ptr());
        ring_guard.unlock();
    }
}

#[no_mangle]
pub extern "C" fn rust_pg_exception_note_phase(
    phase: *const c_char,
    sql: *const c_char,
    stmt: *const c_void,
    db: *const c_void,
) {
    unsafe {
        let mut phase_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_PHASE_RING_MUTEX));

        let slot = &mut EXC_PHASE_RING[EXC_PHASE_RING_NEXT as usize];
        libc::snprintf(
            slot.phase.as_mut_ptr(),
            slot.phase.len(),
            b"%.31s\0".as_ptr() as *const c_char,
            if phase.is_null() { UNKNOWN_STR.as_ptr() as *const c_char } else { phase },
        );
        if !sql.is_null() && *sql != 0 {
            libc::snprintf(
                slot.sql.as_mut_ptr(),
                slot.sql.len(),
                b"%.319s\0".as_ptr() as *const c_char,
                sql,
            );
        } else {
            slot.sql[0] = 0;
        }
        slot.stmt = stmt as *mut c_void;
        slot.db = db as *mut c_void;
        slot.tid = libc::pthread_self() as libc::c_ulong;

        EXC_PHASE_RING_NEXT = (EXC_PHASE_RING_NEXT + 1) % (EXC_PHASE_RING_SIZE as c_int);

        phase_guard.unlock();

        let qlen = if !sql.is_null() && *sql != 0 {
            let mut wrote = libc::snprintf(
                ptr::addr_of_mut!(CRASH_LAST_QUERY) as *mut c_char,
                CRASH_QUERY_MAX_LEN,
                b"%.511s\0".as_ptr() as *const c_char,
                sql,
            );
            if wrote < 0 {
                wrote = 0;
            }
            if wrote >= CRASH_QUERY_MAX_LEN as c_int {
                wrote = CRASH_QUERY_MAX_LEN as c_int - 1;
            }
            wrote
        } else {
            CRASH_LAST_QUERY[0] = 0;
            0
        };
        CRASH_LAST_QUERY_LEN.store(qlen, Ordering::SeqCst);

        let plen = if !phase.is_null() && *phase != 0 {
            let mut wrote = libc::snprintf(
                ptr::addr_of_mut!(CRASH_LAST_PHASE) as *mut c_char,
                CRASH_PHASE_MAX_LEN,
                b"%.63s\0".as_ptr() as *const c_char,
                phase,
            );
            if wrote < 0 {
                wrote = 0;
            }
            if wrote >= CRASH_PHASE_MAX_LEN as c_int {
                wrote = CRASH_PHASE_MAX_LEN as c_int - 1;
            }
            wrote
        } else {
            CRASH_LAST_PHASE[0] = 0;
            0
        };
        CRASH_LAST_PHASE_LEN.store(plen, Ordering::SeqCst);

        if trace_last_query_enabled() && !TRACE_LAST_QUERY_PATH.is_null() && qlen > 0 {
            let fd = libc::open(
                TRACE_LAST_QUERY_PATH,
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o644,
            );
            if fd >= 0 {
                if plen > 0 {
                    let _ = libc::write(fd, ptr::addr_of!(CRASH_LAST_PHASE) as *const c_void, plen as usize);
                    let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
                }
                let _ = libc::write(fd, ptr::addr_of!(CRASH_LAST_QUERY) as *const c_void, qlen as usize);
                let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
                libc::close(fd);
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_pg_exception_dump_recent_phases() {
    unsafe {
        let mut phase_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_PHASE_RING_MUTEX));

        libc::fprintf(
            stderr_ptr(),
            b"[EXC_CONTEXT] Recent phases (oldest -> newest):\n\0".as_ptr() as *const c_char,
        );
        for i in 0..EXC_PHASE_RING_SIZE {
            let idx = (EXC_PHASE_RING_NEXT + i as c_int) % (EXC_PHASE_RING_SIZE as c_int);
            let entry = &EXC_PHASE_RING[idx as usize];
            if entry.phase[0] == 0 {
                continue;
            }
            if entry.sql[0] != 0 {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] phase=%s tid=0x%lx stmt=%p db=%p sql=%.200s\n\0".as_ptr()
                        as *const c_char,
                    i as c_int,
                    entry.phase.as_ptr(),
                    entry.tid,
                    entry.stmt,
                    entry.db,
                    entry.sql.as_ptr(),
                );
            } else {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] phase=%s tid=0x%lx stmt=%p db=%p\n\0".as_ptr() as *const c_char,
                    i as c_int,
                    entry.phase.as_ptr(),
                    entry.tid,
                    entry.stmt,
                    entry.db,
                );
            }
        }

        libc::fflush(stderr_ptr());
        phase_guard.unlock();
    }
}

#[no_mangle]
pub extern "C" fn get_exception_tracker(type_name: *const c_char) -> *mut ExceptionTypeTracker {
    rust_get_exception_tracker(type_name)
}

#[no_mangle]
pub extern "C" fn reset_exception_tracking() {
    rust_reset_exception_tracking();
}

#[no_mangle]
pub extern "C" fn get_type_name(tinfo: *mut c_void) -> *const c_char {
    rust_get_type_name(tinfo)
}

#[no_mangle]
pub extern "C" fn reset_symbol_verification() {
    rust_reset_symbol_verification();
}

#[no_mangle]
pub extern "C" fn pg_check_fake_value(p_val: *mut crate::ffi_types::sqlite3_value) -> *mut PgFakeValue {
    rust_pg_check_fake_value(p_val)
}

#[no_mangle]
pub extern "C" fn is_library_db_path(path: *const c_char) -> c_int {
    crate::db_interpose_helpers::rust_is_library_db_path(path)
}

#[no_mangle]
pub extern "C" fn is_blobs_db_path(path: *const c_char) -> c_int {
    crate::db_interpose_helpers::rust_is_blobs_db_path(path)
}

#[no_mangle]
pub extern "C" fn rewrite_blobs_schema_migrations(sql: *const c_char, db_path: *const c_char) -> *mut c_char {
    rust_rewrite_blobs_schema_migrations(sql, db_path)
}

#[no_mangle]
pub extern "C" fn simple_str_replace(str_ptr: *const c_char, old_ptr: *const c_char, new_ptr: *const c_char) -> *mut c_char {
    rust_simple_str_replace(str_ptr, old_ptr, new_ptr)
}

#[no_mangle]
pub extern "C" fn common_load_sqlite_symbols(handle: *mut c_void) {
    rust_common_load_sqlite_symbols(handle);
}

#[no_mangle]
pub extern "C" fn shim_ensure_ready() -> c_int {
    rust_shim_ensure_ready()
}

#[no_mangle]
pub extern "C" fn worker_init() -> c_int {
    rust_worker_init()
}

#[no_mangle]
pub extern "C" fn worker_cleanup() {
    rust_worker_cleanup();
}

#[no_mangle]
pub extern "C" fn delegate_prepare_to_worker(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    stmt: *mut *mut sqlite3_stmt,
    tail: *mut *const c_char,
) -> c_int {
    rust_delegate_prepare_to_worker(db, z_sql, n_byte, stmt, tail)
}

#[no_mangle]
pub extern "C" fn common_atfork_prepare() {
    rust_common_atfork_prepare();
}

#[no_mangle]
pub extern "C" fn common_atfork_parent() {
    rust_common_atfork_parent();
}

#[no_mangle]
pub extern "C" fn common_atfork_child() {
    rust_common_atfork_child();
}

#[no_mangle]
pub extern "C" fn common_check_fork() -> c_int {
    rust_common_check_fork()
}

#[no_mangle]
pub extern "C" fn common_shim_init_modules() {
    rust_common_shim_init_modules();
}

#[no_mangle]
pub extern "C" fn common_shim_cleanup() {
    rust_common_shim_cleanup();
}

#[no_mangle]
pub extern "C" fn common_signal_handler(sig: c_int) {
    rust_common_signal_handler(sig);
}

#[no_mangle]
pub extern "C" fn print_exception_info(
    type_name: *const c_char,
    count: c_int,
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
) -> *mut c_char {
    rust_print_exception_info(type_name, count, thrown_exception, tinfo)
}

#[no_mangle]
pub extern "C" fn common_handle_exception(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
    in_handler_flag: *mut c_int,
    should_call_original: *mut c_int,
) -> c_int {
    rust_common_handle_exception(thrown_exception, tinfo, in_handler_flag, should_call_original)
}

#[no_mangle]
pub extern "C" fn pg_exception_get_last_query() -> *const c_char {
    rust_pg_exception_get_last_query()
}

#[no_mangle]
pub extern "C" fn pg_exception_get_last_column() -> *const c_char {
    rust_pg_exception_get_last_column()
}

#[no_mangle]
pub extern "C" fn pg_exception_note_query(sql: *const c_char) {
    rust_pg_exception_note_query(sql);
}

#[no_mangle]
pub extern "C" fn pg_exception_dump_recent_queries() {
    rust_pg_exception_dump_recent_queries();
}

#[no_mangle]
pub extern "C" fn pg_exception_note_phase(
    phase: *const c_char,
    sql: *const c_char,
    stmt: *const c_void,
    db: *const c_void,
) {
    rust_pg_exception_note_phase(phase, sql, stmt, db);
}

#[no_mangle]
pub extern "C" fn pg_exception_dump_recent_phases() {
    rust_pg_exception_dump_recent_phases();
}

#[cfg(test)]
mod tests {
    use super::{
        rust_common_handle_exception, rust_common_load_sqlite_symbols, rust_get_exception_tracker,
        rust_reset_exception_tracking, rust_simple_str_replace, tls_column_type_calls_ptr,
        tls_last_query_ptr, tls_value_type_calls_ptr, total_exception_count,
    };
    use libc::{c_void, RTLD_DEFAULT, RTLD_LAZY};
    use std::ffi::{CStr, CString};
    use std::sync::atomic::Ordering;

    fn call_replace(input: Option<&str>, old: Option<&str>, new_str: Option<&str>) -> Option<String> {
        let input_cs = input.map(|s| CString::new(s).unwrap());
        let old_cs = old.map(|s| CString::new(s).unwrap());
        let new_cs = new_str.map(|s| CString::new(s).unwrap());

        let ptr = rust_simple_str_replace(
            input_cs
                .as_ref()
                .map_or(std::ptr::null(), |s| s.as_ptr()),
            old_cs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            new_cs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        );

        if ptr.is_null() {
            return None;
        }

        let out = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            libc::free(ptr as *mut c_void);
        }
        Some(out)
    }

    #[test]
    fn common_helpers_simple_str_replace_null_str_returns_none() {
        assert!(call_replace(None, Some("old"), Some("new")).is_none());
    }

    #[test]
    fn common_helpers_simple_str_replace_null_old_returns_none() {
        assert!(call_replace(Some("hello"), None, Some("new")).is_none());
    }

    #[test]
    fn common_helpers_simple_str_replace_null_new_returns_none() {
        assert!(call_replace(Some("hello"), Some("old"), None).is_none());
    }

    #[test]
    fn common_helpers_simple_str_replace_no_match_returns_none() {
        assert!(call_replace(Some("hello world"), Some("xyz"), Some("abc")).is_none());
    }

    #[test]
    fn common_helpers_simple_str_replace_basic_replace() {
        assert_eq!(
            call_replace(Some("hello world"), Some("world"), Some("earth")),
            Some("hello earth".to_string())
        );
    }

    #[test]
    fn common_helpers_simple_str_replace_at_start() {
        assert_eq!(
            call_replace(Some("hello world"), Some("hello"), Some("goodbye")),
            Some("goodbye world".to_string())
        );
    }

    #[test]
    fn common_helpers_simple_str_replace_at_end() {
        assert_eq!(
            call_replace(Some("hello world"), Some("world"), Some("!")),
            Some("hello !".to_string())
        );
    }

    #[test]
    fn common_helpers_simple_str_replace_shorter_with_longer() {
        assert_eq!(
            call_replace(Some("ab"), Some("a"), Some("xyz")),
            Some("xyzb".to_string())
        );
    }

    #[test]
    fn common_helpers_simple_str_replace_longer_with_shorter() {
        assert_eq!(
            call_replace(Some("hello world"), Some("hello"), Some("hi")),
            Some("hi world".to_string())
        );
    }

    #[test]
    fn common_helpers_simple_str_replace_delete_segment() {
        assert_eq!(
            call_replace(Some("hello world"), Some("hello "), Some("")),
            Some("world".to_string())
        );
    }

    #[test]
    fn common_helpers_simple_str_replace_empty_old_prepends() {
        assert_eq!(
            call_replace(Some("hello"), Some(""), Some("X")),
            Some("Xhello".to_string())
        );
    }

    #[test]
    fn common_helpers_simple_str_replace_first_occurrence_only() {
        assert_eq!(
            call_replace(Some("aaa"), Some("a"), Some("b")),
            Some("baa".to_string())
        );
    }

    #[test]
    fn common_helpers_simple_str_replace_sql_transform() {
        assert_eq!(
            call_replace(
                Some("INSERT OR REPLACE INTO tags"),
                Some("INSERT OR REPLACE INTO"),
                Some("INSERT INTO")
            ),
            Some("INSERT INTO tags".to_string())
        );
    }

    #[test]
    fn exception_tracker_increments_for_same_type() {
        rust_reset_exception_tracking();
        let name = CString::new("TestException").unwrap();

        let t1 = rust_get_exception_tracker(name.as_ptr());
        assert!(!t1.is_null());
        assert_eq!(unsafe { (*t1).count }, 1);

        let t2 = rust_get_exception_tracker(name.as_ptr());
        assert!(!t2.is_null());
        assert_eq!(unsafe { (*t2).count }, 2);
    }

    #[test]
    fn exception_tracking_reset_clears_counts() {
        rust_reset_exception_tracking();
        let name = CString::new("ResetException").unwrap();
        let t1 = rust_get_exception_tracker(name.as_ptr());
        assert_eq!(unsafe { (*t1).count }, 1);

        rust_reset_exception_tracking();
        let t2 = rust_get_exception_tracker(name.as_ptr());
        assert_eq!(unsafe { (*t2).count }, 1);
    }

    #[test]
    fn common_handle_exception_increments_total_count() {
        rust_reset_exception_tracking();
        unsafe {
            *tls_column_type_calls_ptr() = 1;
            *tls_value_type_calls_ptr() = 0;
            *tls_last_query_ptr() = std::ptr::null();
        }

        let mut in_handler = 0;
        let mut should_call_original = 0;
        let rc = rust_common_handle_exception(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut in_handler,
            &mut should_call_original,
        );
        assert_eq!(rc, 0);
        assert_eq!(should_call_original, 1);
        assert_eq!(total_exception_count.load(Ordering::SeqCst), 1);

        unsafe {
            *tls_column_type_calls_ptr() = 0;
        }
    }

    #[test]
    fn common_load_sqlite_symbols_sets_pointers() {
        unsafe {
            super::orig_sqlite3_open = None;
            super::orig_sqlite3_prepare_v2 = None;
            super::orig_sqlite3_column_decltype = None;
        }

        rust_common_load_sqlite_symbols(std::ptr::null_mut());
        unsafe {
            let open = super::orig_sqlite3_open;
            let prepare = super::orig_sqlite3_prepare_v2;
            assert!(open.is_none());
            assert!(prepare.is_none());
        }

        let mut handle = std::ptr::null_mut();
        let names = if cfg!(target_os = "macos") {
            vec![
                CString::new("libsqlite3.dylib").unwrap(),
                CString::new("/usr/lib/libsqlite3.dylib").unwrap(),
            ]
        } else {
            vec![
                CString::new("libsqlite3.so.0").unwrap(),
                CString::new("libsqlite3.so").unwrap(),
            ]
        };
        for name in names {
            unsafe {
                handle = libc::dlopen(name.as_ptr(), RTLD_LAZY);
            }
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            handle = RTLD_DEFAULT;
        }

        unsafe {
            rust_common_load_sqlite_symbols(handle);
            let open = super::orig_sqlite3_open;
            let prepare = super::orig_sqlite3_prepare_v2;
            let decltype = super::orig_sqlite3_column_decltype;
            assert!(open.is_some());
            assert!(prepare.is_some());
            assert!(decltype.is_some());
        }

        if handle != RTLD_DEFAULT {
            unsafe {
                libc::dlclose(handle);
            }
        }
    }

    #[test]
    fn tls_state_is_thread_local() {
        unsafe {
            *tls_column_type_calls_ptr() = 111;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            unsafe {
                *tls_column_type_calls_ptr() = 222;
            }
            let val = unsafe { *tls_column_type_calls_ptr() };
            tx.send(val).unwrap();
        })
        .join()
        .unwrap();

        let other = rx.recv().unwrap();
        assert_eq!(other, 222);
        let main_val = unsafe { *tls_column_type_calls_ptr() };
        assert_eq!(main_val, 111);
    }
}
