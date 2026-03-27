use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::db_interpose_conn_utils::{cstr_to_string_or, log_debug, PthreadMutexGuard};
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgStmt, PARAM_BUF_LEN};

mod numeric_binds;
mod support;
mod text_blob_binds;
mod value_binds;

use numeric_binds::{bind_double_impl, bind_int64_impl, bind_int_impl};
use text_blob_binds::{bind_blob64_impl, bind_blob_impl, bind_text64_impl, bind_text_impl};
use value_binds::{bind_null_impl, bind_value_impl};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_MISUSE: c_int = 21;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;

const PMT_BIND_TEXT_ALLOC: c_int = 0;
const PMT_BIND_HEX_ALLOC: c_int = 1;
const PMT_BIND_VALUE_BLOB_ALLOC: c_int = 2;

static BIND_RESET_DISABLED: AtomicI32 = AtomicI32::new(-1);

static PHASE_BIND_INT: &[u8] = b"bind_int\0";
static PHASE_BIND_INT64: &[u8] = b"bind_int64\0";
static PHASE_BIND_DOUBLE: &[u8] = b"bind_double\0";
static PHASE_BIND_TEXT: &[u8] = b"bind_text\0";
static PHASE_BIND_TEXT64: &[u8] = b"bind_text64\0";
static PHASE_BIND_BLOB: &[u8] = b"bind_blob\0";
static PHASE_BIND_BLOB64: &[u8] = b"bind_blob64\0";
static PHASE_BIND_VALUE: &[u8] = b"bind_value\0";
static PHASE_BIND_NULL: &[u8] = b"bind_null\0";

extern "C" {
    static mut orig_sqlite3_bind_int:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, c_int) -> c_int>;
    static mut orig_sqlite3_bind_int64:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, i64) -> c_int>;
    static mut orig_sqlite3_bind_double:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, f64) -> c_int>;
    static mut orig_sqlite3_bind_text: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_char, c_int, *mut c_void) -> c_int,
    >;
    static mut orig_sqlite3_bind_text64: Option<
        unsafe extern "C" fn(
            *mut sqlite3_stmt,
            c_int,
            *const c_char,
            u64,
            *mut c_void,
            c_uchar,
        ) -> c_int,
    >;
    static mut orig_sqlite3_bind_blob: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_void, c_int, *mut c_void) -> c_int,
    >;
    static mut orig_sqlite3_bind_blob64: Option<
        unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const c_void, u64, *mut c_void) -> c_int,
    >;
    static mut orig_sqlite3_bind_value:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int, *const sqlite3_value) -> c_int>;
    static mut orig_sqlite3_bind_null:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt, c_int) -> c_int>;
    static mut orig_sqlite3_reset: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> c_int>;
    static mut orig_sqlite3_sql: Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *const c_char>;
    static mut orig_sqlite3_db_handle:
        Option<unsafe extern "C" fn(*mut sqlite3_stmt) -> *mut sqlite3>;

    fn pg_find_any_stmt(stmt: *mut sqlite3_stmt) -> *mut PgStmt;
    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_int(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: c_int,
) -> c_int {
    bind_int_impl(p_stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_int64(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: i64,
) -> c_int {
    bind_int64_impl(p_stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_double(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: f64,
) -> c_int {
    bind_double_impl(p_stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_text(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    bind_text_impl(p_stmt, idx, val, n_bytes, destructor)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_blob(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    bind_blob_impl(p_stmt, idx, val, n_bytes, destructor)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_blob64(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: u64,
    destructor: *mut c_void,
) -> c_int {
    bind_blob64_impl(p_stmt, idx, val, n_bytes, destructor)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_text64(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n_bytes: u64,
    destructor: *mut c_void,
    encoding: c_uchar,
) -> c_int {
    bind_text64_impl(p_stmt, idx, val, n_bytes, destructor, encoding)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_value(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    p_value: *const sqlite3_value,
) -> c_int {
    bind_value_impl(p_stmt, idx, p_value)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_bind_null(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    bind_null_impl(p_stmt, idx)
}
