use std::os::raw::c_char;
use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::log_error;
use crate::ffi_types::{sqlite3_stmt, PgConnection, PgStmt};

#[no_mangle]
pub extern "C" fn rust_stmt_create(
    conn: *mut PgConnection,
    sql: *const c_char,
    shadow_stmt: *mut sqlite3_stmt,
) -> *mut PgStmt {
    let mut stmt = PgStmt::new();
    stmt.conn = conn;
    stmt.shadow_stmt = shadow_stmt;
    stmt.sql = if sql.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe { libc::strdup(sql) }
    };
    stmt.ref_count.store(1, Ordering::Release);

    let stmt_ptr = Box::into_raw(Box::new(stmt));

    // Initialize the recursive mutex (must happen after allocation)
    unsafe {
        let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
        if libc::pthread_mutexattr_init(&mut attr as *mut _) != 0 {
            log_error("pg_stmt_create: pthread_mutexattr_init failed");
            drop(Box::from_raw(stmt_ptr));
            return std::ptr::null_mut();
        }
        libc::pthread_mutexattr_settype(&mut attr as *mut _, libc::PTHREAD_MUTEX_RECURSIVE);
        libc::pthread_mutex_init(&mut (*stmt_ptr).mutex as *mut _, &attr as *const _);
        libc::pthread_mutexattr_destroy(&mut attr as *mut _);
    }

    stmt_ptr
}
