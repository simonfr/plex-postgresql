use std::ffi::CStr;
use std::os::raw::c_char;
use std::os::raw::c_int;
use std::os::raw::c_uint;
use std::fs::File;
use std::io::{BufRead, BufReader};
use crate::db_interpose_prepare_helpers::{
    add_if_not_exists_for_sqlite_ddl, alias_collection_sync_aggregates, prepare_query_loop_tick,
    prepare_simple_hash, prepare_time_ms, simplify_fts_for_sqlite, strip_collate_icu_root,
};
use crate::db_interpose_trace_helpers::{
    getenv_nonempty, list_any_token_in_haystack, list_contains_idx, read_first_line_trimmed,
    trim_first_line,
};
use crate::db_interpose_value_helpers::{
    pg_oid_to_sqlite_type_impl, pg_text_to_double_impl, pg_text_to_int64_impl, pg_text_to_int_impl,
};

static TYPE_INTEGER: &[u8] = b"INTEGER\0";
static TYPE_REAL: &[u8] = b"REAL\0";
static TYPE_TEXT: &[u8] = b"TEXT\0";
static TYPE_BLOB: &[u8] = b"BLOB\0";
static TYPE_NUMERIC: &[u8] = b"NUMERIC\0";
static TYPE_DT_INTEGER_8: &[u8] = b"dt_integer(8)\0";

#[inline]
fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok() }
}

#[inline]
pub(crate) unsafe fn cstr_to_str_or_empty<'a>(ptr: *const c_char) -> &'a str {
    cstr_to_str(ptr).unwrap_or("")
}

#[inline]
fn has_boundary(bytes: &[u8], idx: usize) -> bool {
    if idx >= bytes.len() {
        return true;
    }
    let b = bytes[idx];
    b == b'(' || b.is_ascii_whitespace()
}

#[inline]
fn starts_with_icase(bytes: &[u8], pat: &[u8]) -> bool {
    if bytes.len() < pat.len() {
        return false;
    }
    bytes[..pat.len()].eq_ignore_ascii_case(pat)
}

#[inline]
fn slice_eq_icase(bytes: &[u8], start: usize, pat: &[u8]) -> bool {
    if bytes.len() < start + pat.len() {
        return false;
    }
    bytes[start..start + pat.len()].eq_ignore_ascii_case(pat)
}

fn normalize_sqlite_decltype_impl(input: Option<&str>) -> *const c_char {
    let t = input.unwrap_or("");
    let bytes = t.as_bytes();
    if bytes.is_empty() {
        return TYPE_TEXT.as_ptr() as *const c_char;
    }

    if starts_with_icase(bytes, b"DT_INTEGER") {
        if slice_eq_icase(bytes, 10, b"(8)") {
            return TYPE_DT_INTEGER_8.as_ptr() as *const c_char;
        }
        return TYPE_INTEGER.as_ptr() as *const c_char;
    }

    if starts_with_icase(bytes, b"INTEGER") && has_boundary(bytes, 7) {
        if slice_eq_icase(bytes, 7, b"(8)") {
            return TYPE_DT_INTEGER_8.as_ptr() as *const c_char;
        }
        return TYPE_INTEGER.as_ptr() as *const c_char;
    }

    if starts_with_icase(bytes, b"BIGINT") && has_boundary(bytes, 6) {
        return TYPE_DT_INTEGER_8.as_ptr() as *const c_char;
    }

    if t.eq_ignore_ascii_case("INT8")
        || t.eq_ignore_ascii_case("INT64")
        || t.eq_ignore_ascii_case("LONG")
        || t.eq_ignore_ascii_case("dt_integer(8)")
    {
        return TYPE_DT_INTEGER_8.as_ptr() as *const c_char;
    }

    if t.eq_ignore_ascii_case("boolean") || t.eq_ignore_ascii_case("TIMESTAMP") {
        return TYPE_INTEGER.as_ptr() as *const c_char;
    }

    if t.eq_ignore_ascii_case("FLOAT") || t.eq_ignore_ascii_case("DOUBLE") {
        return TYPE_REAL.as_ptr() as *const c_char;
    }

    if starts_with_icase(bytes, b"VARCHAR") && has_boundary(bytes, 7) {
        return TYPE_TEXT.as_ptr() as *const c_char;
    }

    if t.eq_ignore_ascii_case("STRING") || t.eq_ignore_ascii_case("CHAR") {
        return TYPE_TEXT.as_ptr() as *const c_char;
    }

    if t.eq_ignore_ascii_case("REAL") {
        return TYPE_REAL.as_ptr() as *const c_char;
    }
    if t.eq_ignore_ascii_case("TEXT") {
        return TYPE_TEXT.as_ptr() as *const c_char;
    }
    if t.eq_ignore_ascii_case("BLOB") {
        return TYPE_BLOB.as_ptr() as *const c_char;
    }
    if t.eq_ignore_ascii_case("NUMERIC") {
        return TYPE_NUMERIC.as_ptr() as *const c_char;
    }

    TYPE_TEXT.as_ptr() as *const c_char
}

fn pg_udt_to_sqlite_decltype_impl(input: Option<&str>) -> *const c_char {
    let t = input.unwrap_or("");

    if t == "int4" || t == "int2" || t == "bool" || t == "oid" {
        return TYPE_INTEGER.as_ptr() as *const c_char;
    }
    if t == "int8" {
        return TYPE_DT_INTEGER_8.as_ptr() as *const c_char;
    }

    if t == "float4" || t == "float8" || t == "numeric" {
        return TYPE_REAL.as_ptr() as *const c_char;
    }

    if t == "text"
        || t == "varchar"
        || t == "bpchar"
        || t == "name"
        || t == "tsvector"
        || t == "interval"
    {
        return TYPE_TEXT.as_ptr() as *const c_char;
    }

    if t == "timestamp" || t == "timestamptz" {
        return TYPE_INTEGER.as_ptr() as *const c_char;
    }

    if t == "bytea" {
        return TYPE_BLOB.as_ptr() as *const c_char;
    }

    TYPE_TEXT.as_ptr() as *const c_char
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn push_capped(buf: &mut Vec<u8>, out_cap: usize, bytes: &[u8]) {
    if out_cap == 0 || bytes.is_empty() {
        return;
    }
    let remaining = out_cap.saturating_sub(buf.len());
    if remaining == 0 {
        return;
    }
    let take = remaining.min(bytes.len());
    buf.extend_from_slice(&bytes[..take]);
}

fn starts_with_ascii_icase_at(haystack: &[u8], at: usize, pat: &[u8]) -> bool {
    if haystack.len() < at + pat.len() {
        return false;
    }
    haystack[at..at + pat.len()].eq_ignore_ascii_case(pat)
}

fn contains_ascii_icase(haystack: &[u8], pat: &[u8]) -> bool {
    if pat.is_empty() || haystack.len() < pat.len() {
        return false;
    }
    haystack
        .windows(pat.len())
        .any(|w| w.eq_ignore_ascii_case(pat))
}

fn find_ascii_icase_from(haystack: &[u8], start: usize, pat: &[u8]) -> Option<usize> {
    if pat.is_empty() || haystack.len() < pat.len() || start >= haystack.len() {
        return None;
    }
    let mut i = start;
    while i + pat.len() <= haystack.len() {
        if haystack[i..i + pat.len()].eq_ignore_ascii_case(pat) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_prev_numeric_boundary(prev: u8) -> bool {
    matches!(
        prev,
        b'=' | b'>' | b'<' | b' ' | b'(' | b',' | b'+' | b'-' | b'*' | b'/' | b'%'
    )
}

fn is_next_numeric_boundary(bytes: &[u8], i: usize) -> bool {
    if i >= bytes.len() {
        return true;
    }
    let b = bytes[i];
    if matches!(
        b,
        b' ' | b')' | b',' | b';' | b'>' | b'<' | b'=' | b'+' | b'-' | b'*' | b'/'
    ) {
        return true;
    }
    starts_with_ascii_icase_at(bytes, i, b" AND")
        || starts_with_ascii_icase_at(bytes, i, b" OR")
        || starts_with_ascii_icase_at(bytes, i, b" ORDER")
        || starts_with_ascii_icase_at(bytes, i, b" LIMIT")
        || starts_with_ascii_icase_at(bytes, i, b" GROUP")
}

fn normalize_sql_literals_impl(sql: &str) -> Option<(String, Vec<String>)> {
    const MAX_NORMALIZED_PARAMS: usize = 32;

    let bytes = sql.as_bytes();
    if bytes.len() >= 6 && bytes[..6].eq_ignore_ascii_case(b"INSERT") {
        return None;
    }
    if !contains_ascii_icase(bytes, b"WHERE") {
        return None;
    }

    let mut out = String::with_capacity(sql.len() + MAX_NORMALIZED_PARAMS * 4);
    let mut params: Vec<String> = Vec::with_capacity(MAX_NORMALIZED_PARAMS);
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        if params.len() < MAX_NORMALIZED_PARAMS {
            let b = bytes[i];
            let is_number_start = b.is_ascii_digit()
                || (b == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit());
            if is_number_start && !in_single && !in_double {
                let prev = if i == 0 { b' ' } else { bytes[i - 1] };
                if is_prev_numeric_boundary(prev) {
                    let num_start = i;
                    if bytes[i] == b'-' {
                        i += 1;
                    }
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                        i += 1;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }

                    if is_next_numeric_boundary(bytes, i) {
                        let lit = &sql[num_start..i];
                        params.push(lit.to_string());
                        out.push('$');
                        out.push_str(&(params.len()).to_string());
                        continue;
                    }
                    i = num_start;
                }
            }
        }

        let b = bytes[i];
        out.push(b as char);
        if b == b'\'' && !in_double {
            in_single = !in_single;
        } else if b == b'"' && !in_single {
            in_double = !in_double;
        }
        i += 1;
    }

    if params.is_empty() {
        return None;
    }
    Some((out, params))
}

fn is_library_db_path_impl(path: &str) -> bool {
    path.as_bytes()
        .ends_with(b"com.plexapp.plugins.library.db")
}

fn is_library_or_blobs_db_path_impl(path: &str) -> bool {
    contains_ascii_icase(path.as_bytes(), b"com.plexapp.plugins.library.db")
        || contains_ascii_icase(path.as_bytes(), b"com.plexapp.plugins.library.blobs.db")
}

fn is_blobs_db_path_impl(path: &str) -> bool {
    contains_ascii_icase(path.as_bytes(), b"com.plexapp.plugins.library.blobs.db")
}

fn contains_binary_bytes_impl(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    for (i, c) in data.iter().copied().enumerate() {
        if c < 0x20 && c != 0x09 && c != 0x0A && c != 0x0D {
            return true;
        }
        if c == 0x7F || c == 0xC0 || c == 0xC1 || c >= 0xF5 {
            return true;
        }
        if i == 0 && data.len() >= 2 && c == 0x1F && data[1] == 0x8B {
            return true;
        }
    }
    false
}

fn bytes_to_pg_hex_impl(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(2 + data.len() * 2);
    out.push('\\');
    out.push('x');
    for b in data {
        use std::fmt::Write;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}

fn is_related_items_query_impl(pg_sql: &str) -> bool {
    pg_sql.contains("taggings as related")
}

fn should_mask_collection_metadata_type_impl(pg_sql: &str, col_name: &str, raw_val: i64) -> bool {
    raw_val == 18 && col_name.contains("metadata_type") && is_related_items_query_impl(pg_sql)
}

fn find_insert_column_index_impl(sql: &str, column_name: &str) -> i32 {
    if column_name.is_empty() {
        return -1;
    }
    let bytes = sql.as_bytes();
    if !(contains_ascii_icase(bytes, b"INSERT") && contains_ascii_icase(bytes, b"INTO")) {
        return -1;
    }
    let Some(cols_open) = find_ascii_icase(bytes, b"(") else {
        return -1;
    };
    let Some(cols_close) = find_closing_paren(bytes, cols_open + 1) else {
        return -1;
    };
    let cols_section = &sql[cols_open + 1..cols_close];
    let cols = split_csv_simple(cols_section);
    for (i, c) in cols.iter().enumerate() {
        if normalize_ident_token(c).eq_ignore_ascii_case(column_name) {
            return i as i32;
        }
    }
    -1
}

fn find_ascii_icase(haystack: &[u8], pat: &[u8]) -> Option<usize> {
    find_ascii_icase_from(haystack, 0, pat)
}

fn find_closing_paren(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b')' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn split_csv_simple(section: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = section.as_bytes();
    let mut i = 0usize;
    let mut start = 0usize;
    let mut in_single = false;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            in_single = !in_single;
        } else if bytes[i] == b',' && !in_single {
            out.push(section[start..i].trim());
            start = i + 1;
        }
        i += 1;
    }
    if start <= section.len() {
        out.push(section[start..].trim());
    }
    out
}

fn normalize_ident_token(t: &str) -> &str {
    let t = t.trim();
    let t = t.strip_prefix('"').unwrap_or(t);
    let t = t.strip_prefix('`').unwrap_or(t);
    let t = t.strip_suffix('"').unwrap_or(t);
    t.strip_suffix('`').unwrap_or(t)
}

fn is_junk_metadata_insert_impl(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    if !(contains_ascii_icase(bytes, b"INSERT") && contains_ascii_icase(bytes, b"metadata_items")) {
        return false;
    }
    if contains_ascii_icase(bytes, b"metadata_item_settings")
        || contains_ascii_icase(bytes, b"metadata_item_views")
        || contains_ascii_icase(bytes, b"metadata_item_accounts")
        || contains_ascii_icase(bytes, b"metadata_item_clusters")
    {
        return false;
    }

    let Some(cols_open) = find_ascii_icase(bytes, b"(") else {
        return false;
    };
    let Some(cols_close) = find_closing_paren(bytes, cols_open + 1) else {
        return false;
    };
    let cols_section = &sql[cols_open + 1..cols_close];
    let cols = split_csv_simple(cols_section);
    if cols.is_empty() {
        return false;
    }

    let mut lib_idx: Option<usize> = None;
    let mut type_idx: Option<usize> = None;
    for (i, c) in cols.iter().enumerate() {
        let c = normalize_ident_token(c);
        if c.eq_ignore_ascii_case("library_section_id") {
            lib_idx = Some(i);
        }
        if c.eq_ignore_ascii_case("metadata_type") {
            type_idx = Some(i);
        }
    }
    let (Some(lib_idx), Some(type_idx)) = (lib_idx, type_idx) else {
        return false;
    };

    let Some(values_pos) = find_ascii_icase(bytes, b"VALUES") else {
        return false;
    };
    let values_bytes = &bytes[values_pos..];
    let Some(v_open_rel) = find_ascii_icase(values_bytes, b"(") else {
        return false;
    };
    let v_open = values_pos + v_open_rel;
    let Some(v_close) = find_closing_paren(bytes, v_open + 1) else {
        return false;
    };
    let values_section = &sql[v_open + 1..v_close];
    let vals = split_csv_simple(values_section);
    if lib_idx >= vals.len() || type_idx >= vals.len() {
        return false;
    }

    let lib_is_null = vals[lib_idx].trim_start().to_ascii_uppercase().starts_with("NULL");
    let type_is_null = vals[type_idx]
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("NULL");
    lib_is_null && type_is_null
}

fn write_buf(out: *mut c_char, out_len: usize, value: Option<&str>) {
    if out.is_null() || out_len == 0 {
        return;
    }
    unsafe {
        *out = 0;
    }
    let Some(value) = value else {
        return;
    };
    let bytes = value.as_bytes();
    let n = bytes.len().min(out_len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, n);
        *out.add(n) = 0;
    }
}

fn format_epoch_to_datetime_utc_impl(epoch: i64, out: *mut c_char, out_len: usize) -> c_int {
    if out.is_null() || out_len == 0 || epoch <= 0 {
        return 0;
    }

    let t = epoch as libc::time_t;
    let mut tm_utc: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::gmtime_r(&t, &mut tm_utc) };
    if ok.is_null() {
        return 0;
    }

    let fmt = b"%Y-%m-%d %H:%M:%S\0";
    let written = unsafe {
        libc::strftime(
            out as *mut libc::c_char,
            out_len,
            fmt.as_ptr() as *const libc::c_char,
            &tm_utc,
        )
    };
    i32::from(written != 0)
}

#[repr(C)]
pub struct RustNormalizedSql {
    pub normalized_sql: *mut c_char,
    pub param_values: *mut *mut c_char,
    pub param_count: c_int,
}


#[no_mangle]
pub extern "C" fn rust_decltype_hash(ptr: *const c_char) -> u32 {
    let mut hash: u32 = 5381;
    let s = cstr_to_str(ptr).unwrap_or("");
    for b in s.as_bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(*b as u32);
    }
    hash
}

#[no_mangle]
pub extern "C" fn rust_pg_udt_to_sqlite_decltype(ptr: *const c_char) -> *const c_char {
    pg_udt_to_sqlite_decltype_impl(cstr_to_str(ptr))
}

#[no_mangle]
pub extern "C" fn rust_normalize_sqlite_decltype(ptr: *const c_char) -> *const c_char {
    normalize_sqlite_decltype_impl(cstr_to_str(ptr))
}

#[no_mangle]
pub extern "C" fn rust_prepare_simple_hash(ptr: *const c_char, max_len: i32) -> u32 {
    let s = cstr_to_str(ptr).unwrap_or("");
    prepare_simple_hash(s, max_len)
}

#[no_mangle]
pub extern "C" fn rust_prepare_time_ms() -> u64 {
    prepare_time_ms()
}

#[no_mangle]
pub extern "C" fn rust_prepare_query_loop_tick(
    sql: *const c_char,
    count_out: *mut c_int,
    elapsed_ms_out: *mut u64,
) -> c_int {
    if sql.is_null() {
        return 0;
    }
    let s = cstr_to_str(sql).unwrap_or("");
    let (detected, count, elapsed) = match prepare_query_loop_tick(s) {
        Some((count, elapsed)) => (1, count, elapsed),
        None => (0, 0, 0),
    };

    if !count_out.is_null() {
        unsafe {
            *count_out = count;
        }
    }
    if !elapsed_ms_out.is_null() {
        unsafe {
            *elapsed_ms_out = elapsed;
        }
    }
    detected
}

#[no_mangle]
pub extern "C" fn rust_maybe_alias_collection_sync_aggregates(
    sqlite_sql: *const c_char,
    pg_sql: *const c_char,
) -> *mut c_char {
    if sqlite_sql.is_null() || pg_sql.is_null() {
        return std::ptr::null_mut();
    }
    let sqlite_sql = match unsafe { CStr::from_ptr(sqlite_sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let pg_sql = match unsafe { CStr::from_ptr(pg_sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    let Some(out) = alias_collection_sync_aggregates(sqlite_sql, pg_sql) else {
        return std::ptr::null_mut();
    };
    match std::ffi::CString::new(out) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn rust_free_cstring(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let _ = std::ffi::CString::from_raw(ptr);
    }
}

#[no_mangle]
pub extern "C" fn rust_strip_collate_icu_root(sql: *const c_char) -> *mut c_char {
    if sql.is_null() {
        return std::ptr::null_mut();
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let Some(out) = strip_collate_icu_root(sql) else {
        return std::ptr::null_mut();
    };
    match std::ffi::CString::new(out) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn rust_is_junk_metadata_insert(sql: *const c_char) -> c_int {
    if sql.is_null() {
        return 0;
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_junk_metadata_insert_impl(sql))
}

#[no_mangle]
pub extern "C" fn rust_is_library_db_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_library_db_path_impl(path))
}

#[no_mangle]
pub extern "C" fn rust_is_library_or_blobs_db_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_library_or_blobs_db_path_impl(path))
}

#[no_mangle]
pub extern "C" fn rust_is_blobs_db_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_blobs_db_path_impl(path))
}

#[no_mangle]
pub extern "C" fn rust_trace_list_contains_idx(list: *const c_char, idx: c_int) -> c_int {
    if list.is_null() {
        return 0;
    }
    let list = match unsafe { CStr::from_ptr(list) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(list_contains_idx(list, idx))
}

#[no_mangle]
pub extern "C" fn rust_trace_list_any_token_in_haystack(
    list: *const c_char,
    haystack: *const c_char,
) -> c_int {
    if list.is_null() || haystack.is_null() {
        return 0;
    }
    let list = match unsafe { CStr::from_ptr(list) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let haystack = match unsafe { CStr::from_ptr(haystack) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(list_any_token_in_haystack(list, haystack))
}

#[no_mangle]
pub extern "C" fn rust_simplify_fts_for_sqlite(sql: *const c_char) -> *mut c_char {
    if sql.is_null() {
        return std::ptr::null_mut();
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let Some(out) = simplify_fts_for_sqlite(sql) else {
        return std::ptr::null_mut();
    };
    match std::ffi::CString::new(out) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn rust_add_if_not_exists_for_sqlite_ddl(sql: *const c_char) -> *mut c_char {
    if sql.is_null() {
        return std::ptr::null_mut();
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let Some(out) = add_if_not_exists_for_sqlite_ddl(sql) else {
        return std::ptr::null_mut();
    };
    match std::ffi::CString::new(out) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn rust_format_epoch_to_datetime_utc(
    epoch: i64,
    out: *mut c_char,
    out_len: usize,
) -> c_int {
    format_epoch_to_datetime_utc_impl(epoch, out, out_len)
}

#[no_mangle]
pub extern "C" fn rust_contains_binary_bytes(data: *const u8, len: usize) -> c_int {
    if data.is_null() || len == 0 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    i32::from(contains_binary_bytes_impl(bytes))
}

#[no_mangle]
pub extern "C" fn rust_bytes_to_pg_hex(data: *const u8, len: usize) -> *mut c_char {
    if data.is_null() || len == 0 {
        return match std::ffi::CString::new("") {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        };
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    match std::ffi::CString::new(bytes_to_pg_hex_impl(bytes)) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn rust_is_related_items_query(pg_sql: *const c_char) -> c_int {
    if pg_sql.is_null() {
        return 0;
    }
    let pg_sql = match unsafe { CStr::from_ptr(pg_sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(is_related_items_query_impl(pg_sql))
}

#[no_mangle]
pub extern "C" fn rust_should_mask_collection_metadata_type(
    pg_sql: *const c_char,
    col_name: *const c_char,
    raw_val: i64,
) -> c_int {
    if pg_sql.is_null() || col_name.is_null() {
        return 0;
    }
    let pg_sql = match unsafe { CStr::from_ptr(pg_sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let col_name = match unsafe { CStr::from_ptr(col_name) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(should_mask_collection_metadata_type_impl(pg_sql, col_name, raw_val))
}

#[no_mangle]
pub extern "C" fn rust_find_insert_column_index(
    sql: *const c_char,
    column_name: *const c_char,
) -> c_int {
    if sql.is_null() || column_name.is_null() {
        return -1;
    }
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let column_name = match unsafe { CStr::from_ptr(column_name) }.to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    find_insert_column_index_impl(sql, column_name)
}

#[no_mangle]
pub extern "C" fn rust_pg_oid_to_sqlite_type(oid: c_uint) -> c_int {
    pg_oid_to_sqlite_type_impl(oid)
}

#[no_mangle]
pub extern "C" fn rust_pg_text_to_int(value: *const c_char) -> c_int {
    pg_text_to_int_impl(value)
}

#[no_mangle]
pub extern "C" fn rust_pg_text_to_int64(value: *const c_char) -> i64 {
    pg_text_to_int64_impl(value)
}

#[no_mangle]
pub extern "C" fn rust_pg_text_to_double(value: *const c_char) -> f64 {
    pg_text_to_double_impl(value)
}

#[no_mangle]
pub extern "C" fn rust_read_first_line_trim_to_buf(
    path: *const c_char,
    out: *mut c_char,
    out_len: usize,
) -> c_int {
    if path.is_null() || out.is_null() || out_len < 2 {
        return 0;
    }
    unsafe {
        *out = 0;
    }

    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return 0;
    }
    let Some(trimmed) = trim_first_line(&line) else {
        return 0;
    };

    let bytes = trimmed.as_bytes();
    let n = bytes.len().min(out_len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, n);
        *out.add(n) = 0;
    }
    1
}

#[no_mangle]
pub extern "C" fn rust_trace_prepare_sql_ok(_sql: *const c_char) -> c_int {
    // Keep current behavior from C: prepare SQL tracing is force-enabled.
    1
}

#[no_mangle]
pub extern "C" fn rust_load_badcast_config(
    enabled_out: *mut c_int,
    idx_out: *mut c_char,
    idx_len: usize,
    thread_out: *mut c_char,
    thread_len: usize,
    sql_out: *mut c_char,
    sql_len: usize,
    col_out: *mut c_char,
    col_len: usize,
) -> c_int {
    let enabled = if let Some(v) = getenv_nonempty("PLEX_PG_TRACE_BADCAST") {
        i32::from(v != "0")
    } else if let Some(v) = getenv_nonempty("PLEX_PG_LOG_LEVEL") {
        i32::from(v.eq_ignore_ascii_case("ERROR"))
    } else {
        0
    };

    let idx = getenv_nonempty("PLEX_PG_TRACE_BADCAST_IDX")
        .or_else(|| read_first_line_trimmed("/tmp/plex_pg_trace_badcast_idx"));
    let thread = getenv_nonempty("PLEX_PG_TRACE_BADCAST_THREAD")
        .or_else(|| read_first_line_trimmed("/tmp/plex_pg_trace_badcast_thread"));
    let sql = getenv_nonempty("PLEX_PG_TRACE_BADCAST_SQL_CONTAINS")
        .or_else(|| read_first_line_trimmed("/tmp/plex_pg_trace_badcast_sql_contains"));
    let col = getenv_nonempty("PLEX_PG_TRACE_BADCAST_COL_CONTAINS")
        .or_else(|| read_first_line_trimmed("/tmp/plex_pg_trace_badcast_col_contains"));

    if !enabled_out.is_null() {
        unsafe {
            *enabled_out = enabled;
        }
    }
    write_buf(idx_out, idx_len, idx.as_deref());
    write_buf(thread_out, thread_len, thread.as_deref());
    write_buf(sql_out, sql_len, sql.as_deref());
    write_buf(col_out, col_len, col.as_deref());

    1
}

#[no_mangle]
pub extern "C" fn rust_validate_utf8(ptr: *const c_char, len: usize) -> i32 {
    if ptr.is_null() {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    i32::from(std::str::from_utf8(bytes).is_ok())
}

#[no_mangle]
pub extern "C" fn rust_rewrite_server_library_uri(
    input: *const c_char,
    out: *mut c_char,
    out_len: usize,
) -> i32 {
    if input.is_null() || out.is_null() || out_len < 16 {
        return 0;
    }

    let input_bytes = unsafe { CStr::from_ptr(input).to_bytes() };
    const SERVER_PREFIX: &[u8] = b"server://";
    const NEEDLE: &[u8] = b"/com.plexapp.plugins.library/library/";
    const REPLACEMENT: &[u8] = b"library://";

    if find_subslice(input_bytes, SERVER_PREFIX).is_none() || find_subslice(input_bytes, NEEDLE).is_none() {
        return 0;
    }

    let mut out_buf = Vec::with_capacity(input_bytes.len().min(out_len.saturating_sub(1)));
    let out_cap = out_len.saturating_sub(1);
    let mut in_pos = 0usize;
    let mut rewrites = 0;

    while in_pos < input_bytes.len() {
        let slice = &input_bytes[in_pos..];
        let Some(rel_match) = find_subslice(slice, SERVER_PREFIX) else {
            push_capped(&mut out_buf, out_cap, slice);
            break;
        };

        let match_pos = in_pos + rel_match;
        push_capped(&mut out_buf, out_cap, &input_bytes[in_pos..match_pos]);
        in_pos = match_pos;

        let after_server = in_pos + SERVER_PREFIX.len();
        if after_server > input_bytes.len() {
            break;
        }

        if let Some(rel_needle) = find_subslice(&input_bytes[after_server..], NEEDLE) {
            let needle_pos = after_server + rel_needle;
            let full_prefix_len = (needle_pos - in_pos) + NEEDLE.len();
            push_capped(&mut out_buf, out_cap, REPLACEMENT);
            in_pos += full_prefix_len;
            rewrites += 1;
        } else {
            push_capped(&mut out_buf, out_cap, SERVER_PREFIX);
            in_pos += SERVER_PREFIX.len();
        }
    }

    unsafe {
        if !out_buf.is_empty() {
            std::ptr::copy_nonoverlapping(out_buf.as_ptr(), out as *mut u8, out_buf.len());
        }
        *out.add(out_buf.len()) = 0;
    }

    i32::from(rewrites > 0)
}

#[no_mangle]
pub extern "C" fn rust_normalize_sql_literals(sql: *const c_char) -> *mut RustNormalizedSql {
    if sql.is_null() {
        return std::ptr::null_mut();
    }
    let raw = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let Some((normalized_sql, params)) = normalize_sql_literals_impl(raw) else {
        return std::ptr::null_mut();
    };

    let normalized_sql = match std::ffi::CString::new(normalized_sql) {
        Ok(s) => s.into_raw(),
        Err(_) => return std::ptr::null_mut(),
    };

    let mut param_ptrs: Vec<*mut c_char> = Vec::with_capacity(params.len());
    for p in params {
        match std::ffi::CString::new(p) {
            Ok(s) => param_ptrs.push(s.into_raw()),
            Err(_) => {
                for ptr in param_ptrs {
                    if !ptr.is_null() {
                        unsafe {
                            let _ = std::ffi::CString::from_raw(ptr);
                        }
                    }
                }
                unsafe {
                    let _ = std::ffi::CString::from_raw(normalized_sql);
                }
                return std::ptr::null_mut();
            }
        }
    }

    let mut boxed_params = param_ptrs.into_boxed_slice();
    let param_values = boxed_params.as_mut_ptr();
    let param_count = boxed_params.len() as c_int;
    std::mem::forget(boxed_params);

    Box::into_raw(Box::new(RustNormalizedSql {
        normalized_sql,
        param_values,
        param_count,
    }))
}

#[no_mangle]
pub extern "C" fn rust_free_normalized_sql(n: *mut RustNormalizedSql) {
    if n.is_null() {
        return;
    }

    let n = unsafe { Box::from_raw(n) };
    if !n.normalized_sql.is_null() {
        unsafe {
            let _ = std::ffi::CString::from_raw(n.normalized_sql);
        }
    }

    if !n.param_values.is_null() && n.param_count > 0 {
        let len = n.param_count as usize;
        let slice_ptr = std::ptr::slice_from_raw_parts_mut(n.param_values, len);
        let params = unsafe { Box::from_raw(slice_ptr) };
        for p in params.iter().copied() {
            if !p.is_null() {
                unsafe {
                    let _ = std::ffi::CString::from_raw(p);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};

    #[test]
    fn validate_utf8_accepts_valid_input() {
        let s = "Plex \u{1F4FA}";
        assert_eq!(rust_validate_utf8(s.as_ptr() as *const c_char, s.len()), 1);
    }

    #[test]
    fn validate_utf8_rejects_invalid_input() {
        let invalid = [0xffu8, 0xfeu8];
        assert_eq!(rust_validate_utf8(invalid.as_ptr() as *const c_char, invalid.len()), 0);
    }

    #[test]
    fn rewrite_server_uri_rewrites_expected_prefix() {
        let input = CString::new(
            "server://machine/com.plexapp.plugins.library/library/metadata/123",
        )
        .expect("valid c string");
        let mut out = [0 as c_char; 256];

        assert_eq!(
            rust_rewrite_server_library_uri(input.as_ptr(), out.as_mut_ptr(), out.len()),
            1
        );

        let rewritten = unsafe { CStr::from_ptr(out.as_ptr()) }
            .to_str()
            .expect("utf8 output");
        assert_eq!(rewritten, "library://metadata/123");
    }

    #[test]
    fn rewrite_server_uri_handles_multiple_matches() {
        let input = CString::new(
            "a=server://m1/com.plexapp.plugins.library/library/one;b=server://m2/com.plexapp.plugins.library/library/two",
        )
        .expect("valid c string");
        let mut out = [0 as c_char; 256];

        assert_eq!(
            rust_rewrite_server_library_uri(input.as_ptr(), out.as_mut_ptr(), out.len()),
            1
        );

        let rewritten = unsafe { CStr::from_ptr(out.as_ptr()) }
            .to_str()
            .expect("utf8 output");
        assert_eq!(rewritten, "a=library://one;b=library://two");
    }

    #[test]
    fn normalize_sql_literals_extracts_two_params() {
        let sql = "SELECT * FROM t WHERE id = 123 AND score >= -4.5";
        let (normalized, params) = normalize_sql_literals_impl(sql).expect("expected normalized result");
        assert_eq!(normalized, "SELECT * FROM t WHERE id = $1 AND score >= $2");
        assert_eq!(params, vec!["123".to_string(), "-4.5".to_string()]);
    }

    #[test]
    fn normalize_sql_literals_skips_insert() {
        assert!(normalize_sql_literals_impl("INSERT INTO t VALUES (1)").is_none());
    }

    #[test]
    fn prepare_simple_hash_is_deterministic() {
        let a = prepare_simple_hash("SELECT * FROM t", 200);
        let b = prepare_simple_hash("SELECT * FROM t", 200);
        assert_eq!(a, b);
    }

    #[test]
    fn alias_collection_sync_aggregates_rewrites_select_list() {
        let sqlite = "select count(*), min(year), max(year) from tags join taggings on 1=1 group by tags.id";
        let pg = "SELECT count(*), min(year), max(year) FROM tags JOIN taggings ON true GROUP BY tags.id";
        let out = alias_collection_sync_aggregates(sqlite, pg).expect("should rewrite");
        assert!(out.contains("count(*) AS \"count(*)\""));
        assert!(out.contains("min(year) AS \"min(year)\""));
        assert!(out.contains("max(year) AS \"max(year)\""));
    }

    #[test]
    fn alias_collection_sync_aggregates_noop_for_other_queries() {
        let sqlite = "select id from tags";
        let pg = "SELECT id FROM tags";
        assert!(alias_collection_sync_aggregates(sqlite, pg).is_none());
    }

    #[test]
    fn strip_collate_icu_root_removes_both_forms() {
        let sql = "SELECT * FROM t COLLATE icu_root WHERE x=1";
        let out = strip_collate_icu_root(sql).expect("should strip");
        assert!(!out.to_ascii_lowercase().contains("collate icu_root"));
    }

    #[test]
    fn is_library_db_path_matches_suffix() {
        assert!(is_library_db_path_impl(
            "/x/y/com.plexapp.plugins.library.db"
        ));
        assert!(!is_library_db_path_impl("/x/y/other.db"));
    }

    #[test]
    fn is_library_or_blobs_path_matches_both() {
        assert!(is_library_or_blobs_db_path_impl(
            "/x/y/com.plexapp.plugins.library.db"
        ));
        assert!(is_library_or_blobs_db_path_impl(
            "/x/y/com.plexapp.plugins.library.blobs.db"
        ));
        assert!(!is_library_or_blobs_db_path_impl("/x/y/other.db"));
    }

    #[test]
    fn junk_metadata_insert_detects_null_pair() {
        let sql = "INSERT INTO metadata_items (library_section_id, metadata_type, title) VALUES (NULL, NULL, 'x')";
        assert!(is_junk_metadata_insert_impl(sql));
    }

    #[test]
    fn junk_metadata_insert_ignores_non_null() {
        let sql = "INSERT INTO metadata_items (library_section_id, metadata_type) VALUES (1, NULL)";
        assert!(!is_junk_metadata_insert_impl(sql));
    }

    #[test]
    fn trace_list_contains_idx_matches_values() {
        assert!(list_contains_idx("5,6; 7", 6));
        assert!(!list_contains_idx("5,6; 7", 4));
        assert!(list_contains_idx("all", 999));
    }

    #[test]
    fn trace_list_any_token_in_haystack_matches_token() {
        assert!(list_any_token_in_haystack("tags,collections", "from tags join x"));
        assert!(!list_any_token_in_haystack("abc,def", "from tags join x"));
    }

    #[test]
    fn simplify_fts_for_sqlite_rewrites_match_and_join() {
        let sql = "SELECT * FROM a JOIN fts4_metadata_titles t ON t.rowid=a.id WHERE fts4_metadata_titles.title MATCH 'foo''bar'";
        let out = simplify_fts_for_sqlite(sql).expect("should simplify");
        assert!(!out.to_ascii_lowercase().contains("join fts4_metadata_titles"));
        assert!(out.contains("1=0"));
    }

    #[test]
    fn simplify_fts_for_sqlite_noop_without_fts() {
        assert!(simplify_fts_for_sqlite("SELECT * FROM t").is_none());
    }

    #[test]
    fn add_if_not_exists_for_sqlite_ddl_rewrites_create_table() {
        let sql = "CREATE TABLE tags (id INTEGER)";
        let out = add_if_not_exists_for_sqlite_ddl(sql).expect("should rewrite");
        assert!(out.contains("CREATE TABLE IF NOT EXISTS tags"));
    }

    #[test]
    fn add_if_not_exists_for_sqlite_ddl_rewrites_create_unique_index() {
        let sql = "CREATE UNIQUE INDEX idx_tags ON tags(id)";
        let out = add_if_not_exists_for_sqlite_ddl(sql).expect("should rewrite");
        assert!(out.contains("CREATE UNIQUE INDEX IF NOT EXISTS idx_tags"));
    }

    #[test]
    fn add_if_not_exists_for_sqlite_ddl_noop_if_already_present() {
        let sql = "CREATE INDEX IF NOT EXISTS idx_tags ON tags(id)";
        assert!(add_if_not_exists_for_sqlite_ddl(sql).is_none());
    }

    #[test]
    fn binary_detection_and_hex_encoding_work() {
        assert!(contains_binary_bytes_impl(&[0x1f, 0x8b, 0x08]));
        assert!(!contains_binary_bytes_impl(b"hello"));
        assert_eq!(bytes_to_pg_hex_impl(&[0x41, 0x42, 0xff]), "\\x4142ff");
    }

    #[test]
    fn related_items_and_mask_predicates_work() {
        let sql = "select * from taggings as related join x";
        assert!(is_related_items_query_impl(sql));
        assert!(should_mask_collection_metadata_type_impl(
            sql,
            "metadata_type",
            18
        ));
        assert!(!should_mask_collection_metadata_type_impl(
            sql,
            "other_col",
            18
        ));
        assert!(!should_mask_collection_metadata_type_impl(
            "select * from x",
            "metadata_type",
            18
        ));
    }

    #[test]
    fn find_insert_column_index_handles_quoted_columns() {
        let sql = "INSERT INTO metadata_items (\"id\", `library_section_id`, metadata_type, title) VALUES ($1,$2,$3,$4)";
        assert_eq!(find_insert_column_index_impl(sql, "library_section_id"), 1);
        assert_eq!(find_insert_column_index_impl(sql, "metadata_type"), 2);
        assert_eq!(find_insert_column_index_impl(sql, "missing_col"), -1);
    }

    #[test]
    fn pg_oid_to_sqlite_type_mapping_matches_expectations() {
        assert_eq!(pg_oid_to_sqlite_type_impl(20), crate::db_interpose_value_helpers::SQLITE_INTEGER_CONST);
        assert_eq!(pg_oid_to_sqlite_type_impl(701), crate::db_interpose_value_helpers::SQLITE_FLOAT_CONST);
        assert_eq!(pg_oid_to_sqlite_type_impl(17), crate::db_interpose_value_helpers::SQLITE_BLOB_CONST);
        assert_eq!(pg_oid_to_sqlite_type_impl(25), crate::db_interpose_value_helpers::SQLITE_TEXT_CONST);
    }

    #[test]
    fn trim_first_line_trims_ws_and_newline() {
        assert_eq!(
            trim_first_line("  abc \r\n").as_deref(),
            Some("abc")
        );
        assert_eq!(trim_first_line("   \n"), None);
    }
}
