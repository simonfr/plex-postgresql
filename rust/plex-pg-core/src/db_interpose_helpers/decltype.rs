use std::os::raw::c_char;

use super::{has_boundary, slice_eq_icase, starts_with_icase};

static TYPE_INTEGER: &[u8] = b"INTEGER\0";
static TYPE_REAL: &[u8] = b"REAL\0";
static TYPE_TEXT: &[u8] = b"TEXT\0";
static TYPE_BLOB: &[u8] = b"BLOB\0";
static TYPE_NUMERIC: &[u8] = b"NUMERIC\0";
// Use "BIGINT" — SOCI's describe_column strips non-alnum chars, so
// "dt_integer(8)" → "dt_integer" which is NOT in SOCI's type map.
// "BIGINT" maps to db_int64 directly, no step+probe fallback.
static TYPE_DT_INTEGER_8: &[u8] = b"BIGINT\0";

pub(crate) fn normalize_sqlite_decltype_impl(input: Option<&str>) -> *const c_char {
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

pub(crate) fn pg_udt_to_sqlite_decltype_impl(input: Option<&str>) -> *const c_char {
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
