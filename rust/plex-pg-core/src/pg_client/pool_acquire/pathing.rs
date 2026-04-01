use std::os::raw::{c_char, c_void};

use crate::db_interpose_conn_utils::log_debug;
use crate::db_interpose_helpers::cstr_to_str_or_empty;
use crate::sync_utils::mutex_lock;

use super::super::pool_lookup::{is_blobs_db, is_library_db, select_library_pool_path};
use super::super::PoolManager;
use crate::log_debug_lazy;

pub(super) fn resolve_selected_pool_path(
    pm: &PoolManager,
    db_path: *const c_char,
    exclude_conn: *const c_void,
) -> Option<String> {
    let raw_path = if db_path.is_null() {
        ""
    } else {
        unsafe { cstr_to_str_or_empty(db_path) }
    };

    let cached_library_path = if raw_path.is_empty() {
        mutex_lock(&pm.library_db_path).clone()
    } else {
        None
    };
    let selected_path = match select_library_pool_path(raw_path, cached_library_path.as_deref()) {
        Some(path) => path,
        None => {
            if !raw_path.is_empty() {
                log_debug_lazy!(
                    "Pool: skipping non-library db_path for acquisition: {}",
                    raw_path
                );
            } else if !exclude_conn.is_null() {
                log_debug(
                    "Pool: alternate acquisition has empty db_path and no cached library path",
                );
            }
            return None;
        }
    };

    if is_blobs_db(raw_path) && selected_path != raw_path {
        log_debug_lazy!(
            "Pool: canonicalized blobs db_path {} to shared pool path {}",
            raw_path,
            selected_path
        );
    }

    if raw_path.is_empty() && !exclude_conn.is_null() {
        log_debug_lazy!(
            "Pool: alternate acquisition using cached library path {}",
            selected_path
        );
    }

    if !is_library_db(&selected_path) {
        return None;
    }

    Some(selected_path)
}

pub(super) fn remember_library_path(pm: &PoolManager, selected_path: &str) {
    let mut lib_path = mutex_lock(&pm.library_db_path);
    if lib_path.is_none() && !selected_path.is_empty() {
        *lib_path = Some(selected_path.to_string());
    }
}
