use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::{LazyLock, RwLock};

static DECLTYPE_STRINGS: LazyLock<RwLock<HashMap<String, CString>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn intern_decltype(input: &str) -> *const c_char {
    if input.is_empty() {
        return std::ptr::null();
    }

    let mut cache = match DECLTYPE_STRINGS.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if let Some(existing) = cache.get(input) {
        return existing.as_ptr();
    }

    let Ok(value) = CString::new(input) else {
        return std::ptr::null();
    };
    let ptr = value.as_ptr();
    cache.insert(input.to_string(), value);
    ptr
}

pub(crate) fn normalize_sqlite_decltype_impl(input: Option<&str>) -> *const c_char {
    intern_decltype(input.unwrap_or(""))
}

pub(crate) fn pg_udt_to_sqlite_decltype_impl(input: Option<&str>) -> *const c_char {
    let fallback = match input.unwrap_or("") {
        "int8" | "timestamp" | "timestamptz" => "dt_integer(8)",
        "int2" | "int4" | "oid" => "integer",
        "bool" => "boolean",
        "float4" | "float8" | "numeric" => "float",
        "bytea" => "blob",
        "text" | "varchar" | "bpchar" | "name" | "tsvector" | "interval" => "varchar(255)",
        _ => "text",
    };
    intern_decltype(fallback)
}
