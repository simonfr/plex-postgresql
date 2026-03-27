use super::*;
use crate::db_interpose_value::support::{
    fake_value_has_result, helpers_result_ptr, load_fake_value_context, sqlite_type_name,
};
use crate::log_info_lazy;

pub(super) fn value_type_impl(p_val: *mut sqlite3_value) -> c_int {
    unsafe {
        pg_exception_note_phase(
            b"value_type\0".as_ptr() as *const c_char,
            ptr::null(),
            p_val as *mut sqlite3_stmt,
            ptr::null_mut(),
        );
        global_value_type_calls = global_value_type_calls.wrapping_add(1);
        let tls_calls = tls_value_type_calls_ptr();
        *tls_calls = (*tls_calls).wrapping_add(1);
    }

    if p_val.is_null() {
        return SQLITE_NULL;
    }

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_TYPE") }) else {
        return unsafe {
            orig_sqlite3_value_type
                .map(|f| f(p_val))
                .unwrap_or(SQLITE_NULL)
        };
    };

    let call_num = VALUE_TYPE_CALLS.fetch_add(1, Ordering::Relaxed);
    unsafe {
        last_query_being_processed = (*ctx.pg_stmt).pg_sql;
        let tls_query = tls_last_query_ptr();
        *tls_query = (*ctx.pg_stmt).pg_sql;
    }

    let _guard = unsafe { PthreadMutexGuard::lock(&mut (*ctx.pg_stmt).mutex as *mut _) };
    if unsafe { !fake_value_has_result(&ctx) } {
        log_info_lazy!(
            "VALUE_TYPE[{}]: FAKE VALUE but no result (row={} col={})",
            call_num, ctx.row, ctx.col
        );
        return SQLITE_NULL;
    }

    let result_ptr = unsafe { (*ctx.pg_stmt).result };
    let mut is_null = 0;
    let mut oid = 0u32;
    let mut sqlite_type = SQLITE_NULL;
    let ok = crate::db_interpose_helpers::rust_pg_result_type_info(
        helpers_result_ptr(result_ptr),
        ctx.row,
        ctx.col,
        &mut oid as *mut u32,
        &mut is_null as *mut c_int,
        &mut sqlite_type as *mut c_int,
    );
    let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
        helpers_result_ptr(result_ptr),
        ctx.col,
    );
    unsafe {
        last_column_being_accessed = col_name;
    }

    let result = if ok != 0 { sqlite_type } else { SQLITE_NULL };
    if call_num % 1000 == 0 {
        log_info_lazy!(
            "VALUE_TYPE[{}]: col='{}' row={} OID={} is_null={} -> {} sql={}",
            call_num,
            cstr_to_string_or(col_name, "?"),
            ctx.row,
            oid,
            is_null,
            sqlite_type_name(result),
            cstr_prefix(unsafe { (*ctx.pg_stmt).pg_sql }, 60, "?")
        );
    }
    result
}

pub(super) fn value_text_impl(p_val: *mut sqlite3_value) -> *const c_uchar {
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

    let Some(ctx) = (unsafe { load_fake_value_context(p_val, "VALUE_TEXT") }) else {
        return unsafe {
            orig_sqlite3_value_text
                .map(|f| f(p_val))
                .unwrap_or(ptr::null())
        };
    };

    let call_num = VALUE_TEXT_CALLS.fetch_add(1, Ordering::Relaxed);
    let _guard = unsafe { PthreadMutexGuard::lock(&mut (*ctx.pg_stmt).mutex as *mut _) };
    if unsafe { !fake_value_has_result(&ctx) } {
        return ptr::null();
    }

    let result_ptr = unsafe { (*ctx.pg_stmt).result };
    let buf = VALUE_TEXT_IDX.fetch_add(1, Ordering::Relaxed) & 0xFF;
    let len = unsafe {
        crate::db_interpose_helpers::rust_pg_result_text_copy(
            helpers_result_ptr(result_ptr),
            ctx.row,
            ctx.col,
            VALUE_TEXT_BUFFERS[buf].as_mut_ptr() as *mut c_char,
            VALUE_TEXT_BUFFERS[buf].len(),
        )
    };
    if len < 0 {
        if call_num % 100 == 0 {
            log_info_lazy!(
                "VALUE_TEXT[{}]: col={} row={} -> NULL (is_null)",
                call_num, ctx.col, ctx.row
            );
        }
        return ptr::null();
    }

    if call_num % 100 == 0 {
        let col_name = crate::db_interpose_helpers::rust_pg_result_col_name(
            helpers_result_ptr(result_ptr),
            ctx.col,
        );
        let suffix = if len > 30 { "..." } else { "" };
        log_info_lazy!(
            "VALUE_TEXT[{}]: col='{}' row={} val='{:.30}{}'",
            call_num,
            cstr_to_string_or(col_name, "?"),
            ctx.row,
            unsafe { CStr::from_ptr(VALUE_TEXT_BUFFERS[buf].as_ptr() as *const c_char) }
                .to_string_lossy(),
            suffix
        );
    }

    (unsafe { VALUE_TEXT_BUFFERS[buf].as_ptr() }) as *const c_uchar
}
