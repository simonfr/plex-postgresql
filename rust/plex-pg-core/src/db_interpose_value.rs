use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use crate::db_interpose_common::{
    get_orig_sqlite3_value_blob, get_orig_sqlite3_value_bytes, get_orig_sqlite3_value_double,
    get_orig_sqlite3_value_int, get_orig_sqlite3_value_int64, get_orig_sqlite3_value_text,
    get_orig_sqlite3_value_type, tls_last_query_ptr, tls_value_type_calls_ptr,
    GLOBAL_VALUE_TYPE_CALLS,
};
use crate::db_interpose_conn_utils::{cstr_prefix, cstr_to_string_or, log_error};
use crate::db_interpose_helpers::PGresult as PgResultHelpers;
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgStmt};
use crate::libpq_helpers::PGresult as PgResultLibpq;

mod scalar_accessors;
mod support;
mod type_text;

use scalar_accessors::{
    value_blob_impl, value_bytes_impl, value_double_impl, value_int64_impl, value_int_impl,
};
use type_text::{value_text_impl, value_type_impl};

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;

const VALUE_TEXT_BUF_COUNT: usize = 256;
const VALUE_TEXT_BUF_SIZE: usize = 16 * 1024;
const VALUE_BLOB_BUF_COUNT: usize = 64;
const VALUE_BLOB_BUF_SIZE: usize = 64 * 1024;

static VALUE_TYPE_CALLS: AtomicI64 = AtomicI64::new(0);
static VALUE_TEXT_CALLS: AtomicI64 = AtomicI64::new(0);
static VALUE_INT_CALLS: AtomicI64 = AtomicI64::new(0);

static VALUE_TEXT_IDX: AtomicUsize = AtomicUsize::new(0);
static VALUE_BLOB_IDX: AtomicUsize = AtomicUsize::new(0);

static mut VALUE_TEXT_BUFFERS: [[u8; VALUE_TEXT_BUF_SIZE]; VALUE_TEXT_BUF_COUNT] =
    [[0u8; VALUE_TEXT_BUF_SIZE]; VALUE_TEXT_BUF_COUNT];
static mut VALUE_BLOB_BUFFERS: [[u8; VALUE_BLOB_BUF_SIZE]; VALUE_BLOB_BUF_COUNT] =
    [[0u8; VALUE_BLOB_BUF_SIZE]; VALUE_BLOB_BUF_COUNT];

static NEEDLE_TYPE: &[u8] = b"type\0";

#[repr(C)]
struct PgFakeValue {
    magic: u32,
    pg_stmt: *mut c_void,
    col_idx: c_int,
    row_idx: c_int,
    owner_thread: libc::pthread_t,
}

extern "C" {
    fn pg_check_fake_value(p_val: *mut sqlite3_value) -> *mut PgFakeValue;
    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_type(p_val: *mut sqlite3_value) -> c_int {
    value_type_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_text(p_val: *mut sqlite3_value) -> *const c_uchar {
    value_text_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_int(p_val: *mut sqlite3_value) -> c_int {
    value_int_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_int64(p_val: *mut sqlite3_value) -> i64 {
    value_int64_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_double(p_val: *mut sqlite3_value) -> f64 {
    value_double_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_bytes(p_val: *mut sqlite3_value) -> c_int {
    value_bytes_impl(p_val)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_blob(p_val: *mut sqlite3_value) -> *const c_void {
    value_blob_impl(p_val)
}
