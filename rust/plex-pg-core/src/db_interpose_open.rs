use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::db_interpose_conn_utils::{cstr_to_string_or};
use crate::ffi_types::{sqlite3, PgConnection};
use crate::log_info_lazy;

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

static NEEDLE_LIBRARY_DB: &[u8] = b"com.plexapp.plugins.library.db";

extern "C" {
    static mut orig_sqlite3_open:
        Option<unsafe extern "C" fn(*const c_char, *mut *mut sqlite3) -> c_int>;
    static mut orig_sqlite3_open_v2: Option<
        unsafe extern "C" fn(*const c_char, *mut *mut sqlite3, c_int, *const c_char) -> c_int,
    >;
    static mut orig_sqlite3_close: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int>;
    static mut orig_sqlite3_close_v2: Option<unsafe extern "C" fn(*mut sqlite3) -> c_int>;
    static mut shim_passthrough_only: c_int;
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn should_redirect(filename: *const c_char) -> bool {
    let passthrough = unsafe { shim_passthrough_only };
    crate::pg_config::pg_config_should_redirect(filename, passthrough) != 0
}

unsafe fn handle_conn_path_contains(conn: *mut PgConnection, needle: &[u8]) -> bool {
    if conn.is_null() {
        return false;
    }
    let path_ptr = (*conn).db_path.as_ptr();
    if path_ptr.is_null() {
        return false;
    }
    let bytes = CStr::from_ptr(path_ptr).to_bytes();
    contains_subslice(bytes, needle)
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_open(filename: *const c_char, pp_db: *mut *mut sqlite3) -> c_int {
    let redirect = should_redirect(filename);
    log_info_lazy!(
        "OPEN: {} (redirect={})",
        cstr_to_string_or(filename, "(null)"),
        redirect as i32
    );

    let rc = unsafe {
        orig_sqlite3_open
            .map(|f| f(filename, pp_db))
            .unwrap_or(SQLITE_ERROR)
    };

    if rc == SQLITE_OK && redirect {
        let db = unsafe {
            if pp_db.is_null() {
                ptr::null_mut()
            } else {
                *pp_db
            }
        };
        if !db.is_null() {
            let pg_conn = crate::pg_client::rust_pg_connect(filename, db);
            if !pg_conn.is_null() {
                crate::pg_client::rust_pg_register_connection(pg_conn);
                log_info_lazy!(
                    "PostgreSQL connection established for: {}",
                    cstr_to_string_or(filename, "(null)")
                );
            }
        }
    }

    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_open_v2(
    filename: *const c_char,
    pp_db: *mut *mut sqlite3,
    flags: c_int,
    z_vfs: *const c_char,
) -> c_int {
    let redirect = should_redirect(filename);
    log_info_lazy!(
        "OPEN_V2: {} flags=0x{:x} (redirect={})",
        cstr_to_string_or(filename, "(null)"),
        flags,
        redirect as i32
    );

    let rc = unsafe {
        orig_sqlite3_open_v2
            .map(|f| f(filename, pp_db, flags, z_vfs))
            .unwrap_or(SQLITE_ERROR)
    };

    if rc == SQLITE_OK && redirect {
        let db = unsafe {
            if pp_db.is_null() {
                ptr::null_mut()
            } else {
                *pp_db
            }
        };
        if !db.is_null() {
            let pg_conn = crate::pg_client::rust_pg_connect(filename, db);
            if !pg_conn.is_null() {
                crate::pg_client::rust_pg_register_connection(pg_conn);
                log_info_lazy!(
                    "PostgreSQL connection established for: {}",
                    cstr_to_string_or(filename, "(null)")
                );
            }
        }
    }

    rc
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_close(db: *mut sqlite3) -> c_int {
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    if !handle_conn.is_null() {
        log_info_lazy!("CLOSE: PostgreSQL connection for {}", unsafe {
            cstr_to_string_or((*handle_conn).db_path.as_ptr(), "(null)")
        });

        unsafe {
            if handle_conn_path_contains(handle_conn, NEEDLE_LIBRARY_DB) {
                crate::pg_client::rust_pool_release_for_db(db as *const _ as *const c_void);
            }
        }

        crate::pg_client::rust_pg_unregister_connection(handle_conn);
        crate::pg_client::rust_pg_close(handle_conn);
    }

    unsafe { orig_sqlite3_close.map(|f| f(db)).unwrap_or(SQLITE_ERROR) }
}

#[no_mangle]
pub extern "C" fn rust_my_sqlite3_close_v2(db: *mut sqlite3) -> c_int {
    let handle_conn = crate::pg_client::rust_pg_find_handle_connection(db);
    if !handle_conn.is_null() {
        unsafe {
            if handle_conn_path_contains(handle_conn, NEEDLE_LIBRARY_DB) {
                crate::pg_client::rust_pool_release_for_db(db as *const _ as *const c_void);
            }
        }

        crate::pg_client::rust_pg_unregister_connection(handle_conn);
        crate::pg_client::rust_pg_close(handle_conn);
    }

    unsafe { orig_sqlite3_close_v2.map(|f| f(db)).unwrap_or(SQLITE_ERROR) }
}
