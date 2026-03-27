use super::*;
use crate::log_info_lazy;

pub(super) fn prepare_v2_impl(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    unsafe { ensure_real_sqlite_loaded() };

    unsafe {
        if *tls_in_interpose_call_ptr() != 0 {
            if let Some(prepare) = shim_sqlite3_prepare_v2 {
                return prepare(db, z_sql, n_byte, pp_stmt, pz_tail);
            }
            log_error("CRITICAL: shim_sqlite3_prepare_v2 is NULL during recursive call!");
            return SQLITE_ERROR;
        }

        *tls_in_interpose_call_ptr() = 1;
        let result = super::prepare_v2_internal_impl(db, z_sql, n_byte, pp_stmt, pz_tail, 0);
        *tls_in_interpose_call_ptr() = 0;
        result
    }
}

pub(super) fn prepare_impl(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_v2_impl(db, z_sql, n_byte, pp_stmt, pz_tail)
}

fn utf16_input_len(z_sql: *const c_void, n_byte: c_int) -> usize {
    if z_sql.is_null() {
        return 0;
    }
    if n_byte >= 0 {
        return n_byte as usize;
    }

    let mut utf16_len = 0usize;
    let mut p = z_sql as *const u16;
    unsafe {
        while *p != 0 {
            p = p.add(1);
            utf16_len += 1;
        }
    }
    utf16_len * 2
}

unsafe fn maybe_route_utf16_icu_root(
    db: *mut sqlite3,
    z_sql: *const c_void,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_void,
) -> Option<c_int> {
    if z_sql.is_null() {
        return None;
    }

    let utf16_len = utf16_input_len(z_sql, n_byte);
    if utf16_len == 0 {
        return None;
    }

    let max_len = utf16_len * 2 + 1;
    let mut buf = Vec::with_capacity(max_len);
    let src = z_sql as *const u16;
    let mut i = 0usize;
    while i < utf16_len / 2 && *src.add(i) != 0 {
        let ch = *src.add(i) as u32;
        if ch < 0x80 {
            buf.push(ch as u8);
        } else if ch < 0x800 {
            buf.push((0xC0 | (ch >> 6)) as u8);
            buf.push((0x80 | (ch & 0x3F)) as u8);
        } else {
            buf.push((0xE0 | (ch >> 12)) as u8);
            buf.push((0x80 | ((ch >> 6) & 0x3F)) as u8);
            buf.push((0x80 | (ch & 0x3F)) as u8);
        }
        i += 1;
    }

    if buf.is_empty() || !contains_ascii_icase(&buf, b"collate icu_root") {
        return None;
    }

    let Ok(cs) = CString::new(buf) else {
        return None;
    };

    log_info_lazy!(
        "UTF-16 query with icu_root, routing to UTF-8 handler: {}",
        cstr_prefix(cs.as_ptr(), 200, "NULL")
    );
    let mut tail8: *const c_char = ptr::null();
    let rc = prepare_v2_impl(db, cs.as_ptr(), -1, pp_stmt, &mut tail8);
    if !pz_tail.is_null() {
        *pz_tail = ptr::null();
    }
    Some(rc)
}

pub(super) fn prepare16_v2_impl(
    db: *mut sqlite3,
    z_sql: *const c_void,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_void,
) -> c_int {
    if let Some(rc) = unsafe { maybe_route_utf16_icu_root(db, z_sql, n_byte, pp_stmt, pz_tail) } {
        return rc;
    }

    unsafe {
        if let Some(f) = orig_sqlite3_prepare16_v2 {
            return f(db, z_sql, n_byte, pp_stmt, pz_tail);
        }
        sqlite3_prepare16_v2(db, z_sql, n_byte, pp_stmt, pz_tail)
    }
}

pub(super) fn prepare_v3_impl(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    prep_flags: u32,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    if !z_sql.is_null() && contains_icase_ptr(z_sql, "metadata_items") {
        log_info_lazy!(
            "PREPARE_V3 metadata_items query: {}",
            cstr_prefix(z_sql, 200, "NULL")
        );
    }
    let _ = prep_flags;
    prepare_v2_impl(db, z_sql, n_byte, pp_stmt, pz_tail)
}
