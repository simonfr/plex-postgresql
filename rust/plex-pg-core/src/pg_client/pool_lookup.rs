use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::atomic::Ordering;

use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::ffi_types::PgConnection;
use crate::sync_utils::mutex_lock;

use super::tls_cache::{tls_pool_cache_get, tls_pool_cache_set};
use super::{conn_db_path, conn_is_pg_active, pool, pool_get_connection_inner};
use crate::log_debug_lazy;

pub(super) fn is_library_db(path: &str) -> bool {
    path.ends_with("com.plexapp.plugins.library.db")
}

pub(super) fn is_blobs_db(path: &str) -> bool {
    path.ends_with("com.plexapp.plugins.library.blobs.db")
}

pub(super) fn select_library_pool_path(
    raw_path: &str,
    cached_library_path: Option<&str>,
) -> Option<String> {
    if is_library_db(raw_path) {
        return Some(raw_path.to_string());
    }
    if is_blobs_db(raw_path) {
        if let Some(path) = cached_library_path.filter(|path| is_library_db(path)) {
            return Some(path.to_string());
        }
        return raw_path
            .strip_suffix("com.plexapp.plugins.library.blobs.db")
            .map(|prefix| format!("{prefix}com.plexapp.plugins.library.db"));
    }
    if !raw_path.is_empty() {
        return None;
    }
    cached_library_path
        .filter(|path| is_library_db(path))
        .map(|path| path.to_string())
}

pub(super) fn pool_find_connection_for_db(db_handle: usize, db_path: *const c_char) -> *mut c_void {
    let pm = pool();

    let path_str = if db_path.is_null() {
        ""
    } else {
        unsafe { cstr_to_str_or_empty(db_path) }
    };

    if !is_library_db(path_str) {
        return std::ptr::null_mut();
    }

    let pool_conn = pool_get_connection_inner(db_path);
    if pool_conn.is_null() {
        return std::ptr::null_mut();
    }

    if !conn_is_pg_active(pool_conn as *mut PgConnection) {
        return std::ptr::null_mut();
    }

    let pool_size = pm.pool_size();
    for i in 0..pool_size {
        let slot = &pm.slots[i];
        if slot.conn.load(Ordering::Acquire) == pool_conn {
            pm.db_to_pool.assign(db_handle, i);
            log_debug_lazy!("Tracked db {:x} -> pool slot {}", db_handle, i);
            break;
        }
    }

    if let Some((idx, gen)) = tls_pool_cache_get(0) {
        tls_pool_cache_set(db_handle, idx, gen);
    }

    pool_conn
}

pub(super) fn find_any_library_connection() -> *mut c_void {
    let pm = pool();

    let lib_path = mutex_lock(&pm.library_db_path).clone();
    if let Some(path) = lib_path {
        if let Ok(cs) = CString::new(path) {
            let conn = pool_get_connection_inner(cs.as_ptr());
            if !conn.is_null() && conn_is_pg_active(conn as *mut PgConnection) {
                return conn;
            }
        }
    }

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
