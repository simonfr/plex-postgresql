use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uchar, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use crate::db_interpose_common::{tls_last_query_ptr, tls_value_type_calls_ptr};
use crate::db_interpose_helpers::PGresult as PgResultHelpers;
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgStmt};
use crate::libpq_helpers::PGresult as PgResultLibpq;

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

struct PthreadMutexGuard(*mut libc::pthread_mutex_t);

impl PthreadMutexGuard {
    unsafe fn lock(mutex: *mut libc::pthread_mutex_t) -> Self {
        libc::pthread_mutex_lock(mutex);
        Self(mutex)
    }
}

impl Drop for PthreadMutexGuard {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_unlock(self.0);
        }
    }
}

extern "C" {
    static mut orig_sqlite3_value_type: Option<unsafe extern "C" fn(*mut sqlite3_value) -> c_int>;
    static mut orig_sqlite3_value_text: Option<unsafe extern "C" fn(*mut sqlite3_value) -> *const c_uchar>;
    static mut orig_sqlite3_value_int: Option<unsafe extern "C" fn(*mut sqlite3_value) -> c_int>;
    static mut orig_sqlite3_value_int64: Option<unsafe extern "C" fn(*mut sqlite3_value) -> i64>;
    static mut orig_sqlite3_value_double: Option<unsafe extern "C" fn(*mut sqlite3_value) -> f64>;
    static mut orig_sqlite3_value_bytes: Option<unsafe extern "C" fn(*mut sqlite3_value) -> c_int>;
    static mut orig_sqlite3_value_blob: Option<unsafe extern "C" fn(*mut sqlite3_value) -> *const c_void>;

    static mut last_query_being_processed: *const c_char;
    static mut last_column_being_accessed: *const c_char;
    static mut global_value_type_calls: c_long;

    fn pg_check_fake_value(p_val: *mut sqlite3_value) -> *mut PgFakeValue;
    fn pg_exception_note_phase(
        phase: *const c_char,
        sql: *const c_char,
        stmt: *mut sqlite3_stmt,
        db: *mut sqlite3,
    );
}

fn log_error(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(0, cs.as_ptr());
    }
}

fn log_info(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(1, cs.as_ptr());
    }
}

fn log_debug(msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        crate::pg_logging::rust_logging_write(2, cs.as_ptr());
    }
}

fn cstr_to_string_or(ptr: *const c_char, default: &str) -> String {
    if ptr.is_null() {
        return default.to_string();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

fn cstr_prefix(ptr: *const c_char, max_len: usize, default: &str) -> String {
    if ptr.is_null() {
        return default.to_string();
    }
    let bytes = unsafe { CStr::from_ptr(ptr).to_bytes() };
    let slice = &bytes[..bytes.len().min(max_len)];
    String::from_utf8_lossy(slice).into_owned()
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

fn helpers_result_ptr(result: *mut PgResultLibpq) -> *const PgResultHelpers {
    result as *const PgResultHelpers
}

fn fake_value_thread_ok(fake: *const PgFakeValue) -> bool {
    if fake.is_null() {
        return false;
    }
    unsafe { libc::pthread_equal((*fake).owner_thread, libc::pthread_self()) != 0 }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_type(p_val: *mut sqlite3_value) -> c_int {
    unsafe {
        pg_exception_note_phase(
            b"value_type\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
    }

    unsafe {
        global_value_type_calls = global_value_type_calls.wrapping_add(1);
        let tls_calls = tls_value_type_calls_ptr();
        *tls_calls = (*tls_calls).wrapping_add(1);
    }

    if p_val.is_null() {
        return SQLITE_NULL;
    }

    let fake = unsafe { pg_check_fake_value(p_val) };
    if !fake.is_null() && unsafe { !(*fake).pg_stmt.is_null() } {
        if !fake_value_thread_ok(fake) {
            log_error(&format!("VALUE_TYPE: fake value from different thread (stmt={:p})", unsafe { (*fake).pg_stmt }));
            return SQLITE_NULL;
        }

        let pg_stmt = unsafe { (*fake).pg_stmt as *mut PgStmt };
        let call_num = VALUE_TYPE_CALLS.fetch_add(1, Ordering::Relaxed);

        unsafe {
            last_query_being_processed = (*pg_stmt).pg_sql;
            let tls_query = tls_last_query_ptr();
            *tls_query = (*pg_stmt).pg_sql;
        }

        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        let row = unsafe { (*fake).row_idx };
        let col = unsafe { (*fake).col_idx };
        if unsafe { !(*pg_stmt).result.is_null() }
            && row >= 0
            && row < unsafe { (*pg_stmt).num_rows }
            && col >= 0
            && col < unsafe { (*pg_stmt).num_cols }
        {
            let result_ptr = unsafe { (*pg_stmt).result };
            let mut is_null = 0;
            let mut oid = 0u32;
            let mut sqlite_type = SQLITE_NULL;
            let ok = crate::db_interpose_helpers::rust_pg_result_type_info(
                helpers_result_ptr(result_ptr),
                row,
                col,
                &mut oid as *mut u32,
                &mut is_null as *mut c_int,
                &mut sqlite_type as *mut c_int,
            );
            let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
                helpers_result_ptr(result_ptr),
                col,
            );

            unsafe {
                last_column_being_accessed = col_name;
            }

            let result = if ok != 0 { sqlite_type } else { SQLITE_NULL };
            if call_num % 1000 == 0 {
                log_info(&format!(
                    "VALUE_TYPE[{}]: col='{}' row={} OID={} is_null={} -> {} sql={}",
                    call_num,
                    cstr_to_string_or(col_name, "?"),
                    row,
                    oid,
                    is_null,
                    sqlite_type_name(result),
                    cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 60, "?")
                ));
            }
            return result;
        }

        log_info(&format!(
            "VALUE_TYPE[{}]: FAKE VALUE but no result (row={} col={})",
            call_num, row, col
        ));
        return SQLITE_NULL;
    }

    unsafe { orig_sqlite3_value_type.map(|f| f(p_val)).unwrap_or(SQLITE_NULL) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_text(p_val: *mut sqlite3_value) -> *const c_uchar {
    unsafe {
        pg_exception_note_phase(
            b"value_text\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
    }

    if p_val.is_null() {
        return ptr::null();
    }

    let fake = unsafe { pg_check_fake_value(p_val) };
    if !fake.is_null() && unsafe { !(*fake).pg_stmt.is_null() } {
        if !fake_value_thread_ok(fake) {
            log_error(&format!("VALUE_TEXT: fake value from different thread (stmt={:p})", unsafe { (*fake).pg_stmt }));
            return ptr::null();
        }

        let pg_stmt = unsafe { (*fake).pg_stmt as *mut PgStmt };
        let call_num = VALUE_TEXT_CALLS.fetch_add(1, Ordering::Relaxed);
        let row = unsafe { (*fake).row_idx };
        let col = unsafe { (*fake).col_idx };

        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if unsafe { !(*pg_stmt).result.is_null() }
            && row >= 0
            && row < unsafe { (*pg_stmt).num_rows }
            && col >= 0
            && col < unsafe { (*pg_stmt).num_cols }
        {
            let result_ptr = unsafe { (*pg_stmt).result };
            let buf = VALUE_TEXT_IDX.fetch_add(1, Ordering::Relaxed) & 0xFF;
            let len = unsafe {
                crate::db_interpose_helpers::rust_pg_result_text_copy(
                    helpers_result_ptr(result_ptr),
                    row,
                    col,
                    VALUE_TEXT_BUFFERS[buf].as_mut_ptr() as *mut c_char,
                    VALUE_TEXT_BUFFERS[buf].len(),
                )
            };
            if len < 0 {
                if call_num % 100 == 0 {
                    log_info(&format!(
                        "VALUE_TEXT[{}]: col={} row={} -> NULL (is_null)",
                        call_num, col, row
                    ));
                }
                return ptr::null();
            }

            if call_num % 100 == 0 {
                let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
                    helpers_result_ptr(result_ptr),
                    col,
                );
                let suffix = if len > 30 { "..." } else { "" };
                log_info(&format!(
                    "VALUE_TEXT[{}]: col='{}' row={} val='{:.30}{}'",
                    call_num,
                    cstr_to_string_or(col_name, "?"),
                    row,
                    unsafe { CStr::from_ptr(VALUE_TEXT_BUFFERS[buf].as_ptr() as *const c_char) }
                        .to_string_lossy(),
                    suffix
                ));
            }

            return unsafe { VALUE_TEXT_BUFFERS[buf].as_ptr() } as *const c_uchar;
        }

        return ptr::null();
    }

    unsafe { orig_sqlite3_value_text.map(|f| f(p_val)).unwrap_or(ptr::null()) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_int(p_val: *mut sqlite3_value) -> c_int {
    unsafe {
        pg_exception_note_phase(
            b"value_int\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
    }

    if p_val.is_null() {
        return 0;
    }

    let fake = unsafe { pg_check_fake_value(p_val) };
    if !fake.is_null() && unsafe { !(*fake).pg_stmt.is_null() } {
        if !fake_value_thread_ok(fake) {
            log_error(&format!("VALUE_INT: fake value from different thread (stmt={:p})", unsafe { (*fake).pg_stmt }));
            return 0;
        }

        let pg_stmt = unsafe { (*fake).pg_stmt as *mut PgStmt };
        let _call_num = VALUE_INT_CALLS.fetch_add(1, Ordering::Relaxed);
        let row = unsafe { (*fake).row_idx };
        let col = unsafe { (*fake).col_idx };

        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if unsafe { !(*pg_stmt).result.is_null() }
            && row >= 0
            && row < unsafe { (*pg_stmt).num_rows }
            && col >= 0
            && col < unsafe { (*pg_stmt).num_cols }
        {
            let result_ptr = unsafe { (*pg_stmt).result };
            let result = crate::db_interpose_helpers::rust_pg_result_int(
                helpers_result_ptr(result_ptr),
                row,
                col,
            );

            let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
                helpers_result_ptr(result_ptr),
                col,
            );
            if !col_name.is_null() {
                let needle = NEEDLE_TYPE.as_ptr() as *const c_char;
                if unsafe { !libc::strstr(col_name, needle).is_null() } {
                    let mut raw_buf = [0 as c_char; 128];
                    let mut raw_val = "?".to_string();
                    let raw_len = crate::db_interpose_helpers::rust_pg_result_text_copy(
                        helpers_result_ptr(result_ptr),
                        row,
                        col,
                        raw_buf.as_mut_ptr(),
                        raw_buf.len(),
                    );
                    if raw_len >= 0 {
                        raw_val = cstr_to_string_or(raw_buf.as_ptr(), "?");
                    }
                    log_debug(&format!(
                        "TYPE_DEBUG_VALUE_INT: col='{}' idx={} row={} raw_val='{}' result={} sql={}",
                        cstr_to_string_or(col_name, "?"),
                        col,
                        row,
                        raw_val,
                        result,
                        cstr_prefix(unsafe { (*pg_stmt).pg_sql }, 200, "?")
                    ));
                }
            }

            return result;
        }

        return 0;
    }

    unsafe { orig_sqlite3_value_int.map(|f| f(p_val)).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_int64(p_val: *mut sqlite3_value) -> i64 {
    unsafe {
        pg_exception_note_phase(
            b"value_int64\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
    }

    if p_val.is_null() {
        return 0;
    }

    let fake = unsafe { pg_check_fake_value(p_val) };
    if !fake.is_null() && unsafe { !(*fake).pg_stmt.is_null() } {
        if !fake_value_thread_ok(fake) {
            log_error(&format!("VALUE_INT64: fake value from different thread (stmt={:p})", unsafe { (*fake).pg_stmt }));
            return 0;
        }

        let pg_stmt = unsafe { (*fake).pg_stmt as *mut PgStmt };
        let row = unsafe { (*fake).row_idx };
        let col = unsafe { (*fake).col_idx };

        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if unsafe { !(*pg_stmt).result.is_null() }
            && row >= 0
            && row < unsafe { (*pg_stmt).num_rows }
            && col >= 0
            && col < unsafe { (*pg_stmt).num_cols }
        {
            return crate::db_interpose_helpers::rust_pg_result_int64(
                helpers_result_ptr(unsafe { (*pg_stmt).result }),
                row,
                col,
            ) as i64;
        }

        return 0;
    }

    unsafe { orig_sqlite3_value_int64.map(|f| f(p_val)).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_double(p_val: *mut sqlite3_value) -> f64 {
    if p_val.is_null() {
        return 0.0;
    }

    let fake = unsafe { pg_check_fake_value(p_val) };
    if !fake.is_null() && unsafe { !(*fake).pg_stmt.is_null() } {
        if !fake_value_thread_ok(fake) {
            log_error(&format!("VALUE_DOUBLE: fake value from different thread (stmt={:p})", unsafe { (*fake).pg_stmt }));
            return 0.0;
        }

        let pg_stmt = unsafe { (*fake).pg_stmt as *mut PgStmt };
        let row = unsafe { (*fake).row_idx };
        let col = unsafe { (*fake).col_idx };

        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if unsafe { !(*pg_stmt).result.is_null() }
            && row >= 0
            && row < unsafe { (*pg_stmt).num_rows }
            && col >= 0
            && col < unsafe { (*pg_stmt).num_cols }
        {
            return crate::db_interpose_helpers::rust_pg_result_double(
                helpers_result_ptr(unsafe { (*pg_stmt).result }),
                row,
                col,
            );
        }

        return 0.0;
    }

    unsafe { orig_sqlite3_value_double.map(|f| f(p_val)).unwrap_or(0.0) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_bytes(p_val: *mut sqlite3_value) -> c_int {
    if p_val.is_null() {
        return 0;
    }

    let fake = unsafe { pg_check_fake_value(p_val) };
    if !fake.is_null() && unsafe { !(*fake).pg_stmt.is_null() } {
        if !fake_value_thread_ok(fake) {
            log_error(&format!("VALUE_BYTES: fake value from different thread (stmt={:p})", unsafe { (*fake).pg_stmt }));
            return 0;
        }

        let pg_stmt = unsafe { (*fake).pg_stmt as *mut PgStmt };
        let row = unsafe { (*fake).row_idx };
        let col = unsafe { (*fake).col_idx };

        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if unsafe { !(*pg_stmt).result.is_null() }
            && row >= 0
            && row < unsafe { (*pg_stmt).num_rows }
            && col >= 0
            && col < unsafe { (*pg_stmt).num_cols }
        {
            let len = crate::db_interpose_helpers::rust_pg_result_length(
                helpers_result_ptr(unsafe { (*pg_stmt).result }),
                row,
                col,
            );
            return if len > 0 { len } else { 0 };
        }

        return 0;
    }

    unsafe { orig_sqlite3_value_bytes.map(|f| f(p_val)).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_value_blob(p_val: *mut sqlite3_value) -> *const c_void {
    if p_val.is_null() {
        return ptr::null();
    }

    let fake = unsafe { pg_check_fake_value(p_val) };
    if !fake.is_null() && unsafe { !(*fake).pg_stmt.is_null() } {
        if !fake_value_thread_ok(fake) {
            log_error(&format!("VALUE_INT64: fake value from different thread (stmt={:p})", unsafe { (*fake).pg_stmt }));
            return ptr::null();
        }

        let pg_stmt = unsafe { (*fake).pg_stmt as *mut PgStmt };
        let row = unsafe { (*fake).row_idx };
        let col = unsafe { (*fake).col_idx };

        let _guard = unsafe { PthreadMutexGuard::lock(&mut (*pg_stmt).mutex as *mut _) };

        if unsafe { !(*pg_stmt).result.is_null() }
            && row >= 0
            && row < unsafe { (*pg_stmt).num_rows }
            && col >= 0
            && col < unsafe { (*pg_stmt).num_cols }
        {
            let buf = VALUE_BLOB_IDX.fetch_add(1, Ordering::Relaxed) & 0x3F;
            let result_ptr = unsafe { (*pg_stmt).result };
            let len = unsafe {
                crate::db_interpose_helpers::rust_pg_result_blob_copy(
                    helpers_result_ptr(result_ptr),
                    row,
                    col,
                    VALUE_BLOB_BUFFERS[buf].as_mut_ptr(),
                    VALUE_BLOB_BUFFERS[buf].len() - 1,
                )
            };
            if len <= 0 {
                return ptr::null();
            }
            return unsafe { VALUE_BLOB_BUFFERS[buf].as_ptr() as *const c_void };
        }

        return ptr::null();
    }

    unsafe { orig_sqlite3_value_blob.map(|f| f(p_val)).unwrap_or(ptr::null()) }
}
