use super::*;

pub(crate) fn pg_decode_bytea_cached_impl(
    pg_stmt: *mut PgStmt,
    row: c_int,
    col: c_int,
    out_length: *mut c_int,
) -> *const c_void {
    if pg_stmt.is_null() {
        if !out_length.is_null() {
            unsafe { *out_length = 0 };
        }
        return ptr::null();
    }

    unsafe {
        if (*pg_stmt).decoded_blob_row == row && !(*pg_stmt).decoded_blobs[col as usize].is_null() {
            if !out_length.is_null() {
                *out_length = (*pg_stmt).decoded_blob_lens[col as usize];
            }
            return (*pg_stmt).decoded_blobs[col as usize];
        }

        if (*pg_stmt).decoded_blob_row != row {
            crate::db_interpose_helpers::rust_step_clear_row_caches(
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                (*pg_stmt).decoded_blobs.as_mut_ptr(),
                (*pg_stmt).decoded_blob_lens.as_mut_ptr(),
                (*pg_stmt).decoded_blobs.len() as c_int,
                ptr::null_mut(),
                &mut (*pg_stmt).decoded_blob_row as *mut c_int,
            );
            (*pg_stmt).decoded_blob_row = row;
        }

        let mut decoded: *mut u8 = ptr::null_mut();
        let mut len: c_int = 0;
        let mut is_hex: c_int = 0;
        let mut is_null: c_int = 0;
        let ok = crate::db_interpose_helpers::rust_pg_decode_bytea(
            helpers_result_ptr((*pg_stmt).result),
            row,
            col,
            &mut decoded as *mut *mut u8,
            &mut len as *mut c_int,
            &mut is_hex as *mut c_int,
            &mut is_null as *mut c_int,
        );
        if ok == 0 || is_null != 0 || decoded.is_null() {
            if !out_length.is_null() {
                *out_length = 0;
            }
            return ptr::null();
        }

        if is_hex == 0 {
            if !out_length.is_null() {
                *out_length = len;
            }
            return decoded as *const c_void;
        }

        (*pg_stmt).decoded_blobs[col as usize] = decoded as *mut c_void;
        (*pg_stmt).decoded_blob_lens[col as usize] = len;
        if !out_length.is_null() {
            *out_length = len;
        }

        if crate::pg_mem_telemetry::rust_mem_telemetry_enabled() != 0 {
            crate::pg_mem_telemetry::rust_mem_telemetry_add(
                PMT_COLUMN_DECODED_BLOB_ALLOC,
                (len as u64).saturating_add(1),
                1,
            );
        }

        decoded as *const c_void
    }
}

#[no_mangle]
pub extern "C" fn rust_pg_decode_bytea_cached(
    pg_stmt: *mut PgStmt,
    row: c_int,
    col: c_int,
    out_length: *mut c_int,
) -> *const c_void {
    pg_decode_bytea_cached_impl(pg_stmt, row, col, out_length)
}
