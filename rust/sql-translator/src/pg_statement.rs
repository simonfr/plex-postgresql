/// Module: pg_statement
///
/// Pure, portable logic extracted from `pg_statement.c`, exposed via C FFI.
/// Covers OID→SQLite type/decltype mapping and SQL string transformations.
/// All libpq/sqlite3-dependent code remains in C.
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

// ─── SQLite type constants ────────────────────────────────────────────────────

const SQLITE_INTEGER: i32 = 1;
const SQLITE_FLOAT: i32 = 2;
const SQLITE_TEXT: i32 = 3;
const SQLITE_BLOB: i32 = 4;

// ─── Static decltype byte strings (null-terminated) ──────────────────────────

static DECLTYPE_INTEGER: &[u8] = b"INTEGER\0";
static DECLTYPE_BIGINT: &[u8] = b"BIGINT\0";
static DECLTYPE_REAL: &[u8] = b"REAL\0";
static DECLTYPE_BLOB: &[u8] = b"BLOB\0";
static DECLTYPE_TEXT: &[u8] = b"TEXT\0";

// ─── ON CONFLICT clause appended for metadata_item_settings upserts ──────────

const ON_CONFLICT_CLAUSE: &str = " ON CONFLICT (account_id, guid) DO UPDATE SET \
rating = COALESCE(EXCLUDED.rating, plex.metadata_item_settings.rating), \
view_offset = EXCLUDED.view_offset, \
view_count = CASE WHEN plex.metadata_item_settings.view_count > 0 AND EXCLUDED.view_count = 0 \
THEN 0 ELSE GREATEST(EXCLUDED.view_count, plex.metadata_item_settings.view_count, 1) END, \
last_viewed_at = CASE WHEN plex.metadata_item_settings.view_count > 0 AND EXCLUDED.view_count = 0 \
THEN NULL ELSE COALESCE(EXCLUDED.last_viewed_at, EXTRACT(EPOCH FROM NOW())::bigint) END, \
updated_at = COALESCE(EXCLUDED.updated_at, EXTRACT(EPOCH FROM NOW())::bigint), \
skip_count = EXCLUDED.skip_count, \
last_skipped_at = EXCLUDED.last_skipped_at, \
changed_at = COALESCE(EXCLUDED.changed_at, EXTRACT(EPOCH FROM NOW())::bigint), \
extra_data = COALESCE(EXCLUDED.extra_data, plex.metadata_item_settings.extra_data), \
last_rated_at = COALESCE(EXCLUDED.last_rated_at, plex.metadata_item_settings.last_rated_at) \
RETURNING id";

// ─── Internal pure helpers ────────────────────────────────────────────────────

/// Map a PostgreSQL OID to an SQLite type constant.
/// Returns SQLITE_INTEGER(1), SQLITE_FLOAT(2), SQLITE_TEXT(3), or SQLITE_BLOB(4).
pub(crate) fn oid_to_sqlite_type(oid: u32) -> i32 {
    match oid {
        16          // BOOL
        | 20        // INT8
        | 21        // INT2
        | 23        // INT4
        | 26        // OID
        => SQLITE_INTEGER,

        700         // FLOAT4
        | 701       // FLOAT8
        | 1700      // NUMERIC
        => SQLITE_FLOAT,

        17          // BYTEA
        => SQLITE_BLOB,

        _           // TEXT, BPCHAR, VARCHAR, and everything else
        => SQLITE_TEXT,
    }
}

/// Map a PostgreSQL OID to an SQLite declared-type string.
/// Returns a `&'static CStr` pointing into a static byte array.
///
/// CRITICAL: OID 20 (INT8/bigint) → "BIGINT", not "INTEGER".
/// SOCI maps "INTEGER" → db_int32 (32-bit). Without this, bigint columns
/// would silently truncate via int32→int64 cast (see SOCI Issue #1190).
pub(crate) fn oid_to_sqlite_decltype(oid: u32) -> &'static CStr {
    let bytes: &'static [u8] = match oid {
        16          // BOOL
        | 21        // INT2
        | 23        // INT4
        | 26        // OID
        => DECLTYPE_INTEGER,

        20          // INT8/bigint — must be BIGINT for SOCI 64-bit mapping
        => DECLTYPE_BIGINT,

        700         // FLOAT4
        | 701       // FLOAT8
        | 1700      // NUMERIC
        => DECLTYPE_REAL,

        17          // BYTEA
        => DECLTYPE_BLOB,

        25          // TEXT
        | 1042      // BPCHAR (char)
        | 1043      // VARCHAR
        | 1082      // DATE
        | 1083      // TIME
        | 1114      // TIMESTAMP
        | 1184      // TIMESTAMPTZ
        | _         // default
        => DECLTYPE_TEXT,
    };
    // SAFETY: all byte arrays above are valid null-terminated UTF-8 literals.
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

/// Convert a `metadata_item_settings` INSERT into an upsert by appending the
/// ON CONFLICT clause.
///
/// Returns `Some(String)` when the SQL qualifies (contains "INSERT INTO" and
/// "metadata_item_settings", and does NOT already contain "ON CONFLICT" or
/// "RETURNING").  Returns `None` otherwise.
pub(crate) fn convert_metadata_settings_upsert(sql: &str) -> Option<String> {
    let lower = sql.to_lowercase();
    if !lower.contains("insert into") {
        return None;
    }
    if !lower.contains("metadata_item_settings") {
        return None;
    }
    if lower.contains("on conflict") {
        return None;
    }
    if lower.contains("returning") {
        return None;
    }
    Some(format!("{}{}", sql, ON_CONFLICT_CLAUSE))
}

/// Extract the metadata item ID from a `play_queue_generators` INSERT SQL.
///
/// Looks for a `/metadata/<id>` or `%2Fmetadata%2F<id>` pattern.
/// Returns 0 if the SQL is not a play_queue_generators INSERT or no ID is found.
pub(crate) fn extract_metadata_id(sql: &str) -> i64 {
    let lower = sql.to_lowercase();
    if !lower.contains("play_queue_generators") {
        return 0;
    }
    if !lower.contains("insert") {
        return 0;
    }

    // Try URL-encoded pattern first, then plain
    let pat_encoded = "%2Fmetadata%2F";
    let pat_plain = "/metadata/";

    let after = if let Some(i) = sql.find(pat_encoded) {
        &sql[i + pat_encoded.len()..]
    } else if let Some(i) = sql.find(pat_plain) {
        &sql[i + pat_plain.len()..]
    } else {
        return 0;
    };

    // Parse the decimal digits that immediately follow the pattern
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    digits.parse::<i64>().unwrap_or(0)
}

// ─── FFI helpers ──────────────────────────────────────────────────────────────

/// Safely convert a nullable C string pointer to a `&str`.
/// Returns `""` for NULL or invalid UTF-8.
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    CStr::from_ptr(ptr).to_str().unwrap_or("")
}

// ─── Public C FFI functions ───────────────────────────────────────────────────

/// Map PostgreSQL OID to SQLite type constant.
///
/// Returns:
///   - 1 (`SQLITE_INTEGER`) for bool, int2, int4, int8, oid
///   - 2 (`SQLITE_FLOAT`)   for float4, float8, numeric
///   - 4 (`SQLITE_BLOB`)    for bytea
///   - 3 (`SQLITE_TEXT`)    for text, bpchar, varchar, and all other types
#[no_mangle]
pub extern "C" fn rust_oid_to_sqlite_type(oid: u32) -> i32 {
    oid_to_sqlite_type(oid)
}

/// Map PostgreSQL OID to SQLite declared-type string.
///
/// Returns a pointer to a static null-terminated C string.
/// The caller must NOT free this pointer.
///
/// Notable mapping: OID 20 (int8/bigint) → `"BIGINT"` (not `"INTEGER"`).
/// This is critical for SOCI's 64-bit integer handling.
#[no_mangle]
pub extern "C" fn rust_oid_to_sqlite_decltype(oid: u32) -> *const c_char {
    oid_to_sqlite_decltype(oid).as_ptr()
}

/// Convert a `metadata_item_settings` INSERT statement to upsert form by
/// appending an `ON CONFLICT (account_id, guid) DO UPDATE SET …` clause.
///
/// Returns a newly allocated C string (malloc'd via `CString::into_raw`) that
/// the caller **must** `free()`.  Returns `NULL` when the SQL does not qualify
/// or when `sql` is `NULL`.
#[no_mangle]
pub extern "C" fn rust_convert_metadata_settings_upsert(sql: *const c_char) -> *mut c_char {
    let s = unsafe { cstr_to_str(sql) };
    if s.is_empty() {
        return std::ptr::null_mut();
    }
    match convert_metadata_settings_upsert(s) {
        Some(result) => match CString::new(result) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

/// Extract the metadata item ID from a `play_queue_generators` INSERT SQL.
///
/// Searches for a `%2Fmetadata%2F<id>` or `/metadata/<id>` pattern.
/// Returns 0 when `sql` is `NULL`, the SQL is not a qualifying INSERT, or no
/// metadata ID is found.
#[no_mangle]
pub extern "C" fn rust_extract_metadata_id(sql: *const c_char) -> i64 {
    let s = unsafe { cstr_to_str(sql) };
    if s.is_empty() {
        return 0;
    }
    extract_metadata_id(s)
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a temporary CString and return it so it stays alive
    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    // ── oid_to_sqlite_type ────────────────────────────────────────────────────

    #[test]
    fn type_bool_is_integer() {
        assert_eq!(oid_to_sqlite_type(16), SQLITE_INTEGER);
    }

    #[test]
    fn type_int8_is_integer() {
        assert_eq!(oid_to_sqlite_type(20), SQLITE_INTEGER);
    }

    #[test]
    fn type_int2_is_integer() {
        assert_eq!(oid_to_sqlite_type(21), SQLITE_INTEGER);
    }

    #[test]
    fn type_int4_is_integer() {
        assert_eq!(oid_to_sqlite_type(23), SQLITE_INTEGER);
    }

    #[test]
    fn type_oid_is_integer() {
        assert_eq!(oid_to_sqlite_type(26), SQLITE_INTEGER);
    }

    #[test]
    fn type_float4_is_float() {
        assert_eq!(oid_to_sqlite_type(700), SQLITE_FLOAT);
    }

    #[test]
    fn type_float8_is_float() {
        assert_eq!(oid_to_sqlite_type(701), SQLITE_FLOAT);
    }

    #[test]
    fn type_numeric_is_float() {
        assert_eq!(oid_to_sqlite_type(1700), SQLITE_FLOAT);
    }

    #[test]
    fn type_bytea_is_blob() {
        assert_eq!(oid_to_sqlite_type(17), SQLITE_BLOB);
    }

    #[test]
    fn type_text_is_text() {
        assert_eq!(oid_to_sqlite_type(25), SQLITE_TEXT);
    }

    #[test]
    fn type_bpchar_is_text() {
        assert_eq!(oid_to_sqlite_type(1042), SQLITE_TEXT);
    }

    #[test]
    fn type_varchar_is_text() {
        assert_eq!(oid_to_sqlite_type(1043), SQLITE_TEXT);
    }

    #[test]
    fn type_unknown_oid_is_text() {
        assert_eq!(oid_to_sqlite_type(9999), SQLITE_TEXT);
    }

    // ── oid_to_sqlite_decltype ────────────────────────────────────────────────

    #[test]
    fn decltype_int8_is_bigint() {
        // CRITICAL: must be "BIGINT" not "INTEGER" for SOCI 64-bit correctness
        assert_eq!(oid_to_sqlite_decltype(20).to_str().unwrap(), "BIGINT");
    }

    #[test]
    fn decltype_bool_is_integer() {
        assert_eq!(oid_to_sqlite_decltype(16).to_str().unwrap(), "INTEGER");
    }

    #[test]
    fn decltype_int2_is_integer() {
        assert_eq!(oid_to_sqlite_decltype(21).to_str().unwrap(), "INTEGER");
    }

    #[test]
    fn decltype_int4_is_integer() {
        assert_eq!(oid_to_sqlite_decltype(23).to_str().unwrap(), "INTEGER");
    }

    #[test]
    fn decltype_oid_is_integer() {
        assert_eq!(oid_to_sqlite_decltype(26).to_str().unwrap(), "INTEGER");
    }

    #[test]
    fn decltype_float4_is_real() {
        assert_eq!(oid_to_sqlite_decltype(700).to_str().unwrap(), "REAL");
    }

    #[test]
    fn decltype_float8_is_real() {
        assert_eq!(oid_to_sqlite_decltype(701).to_str().unwrap(), "REAL");
    }

    #[test]
    fn decltype_numeric_is_real() {
        assert_eq!(oid_to_sqlite_decltype(1700).to_str().unwrap(), "REAL");
    }

    #[test]
    fn decltype_bytea_is_blob() {
        assert_eq!(oid_to_sqlite_decltype(17).to_str().unwrap(), "BLOB");
    }

    #[test]
    fn decltype_text_is_text() {
        assert_eq!(oid_to_sqlite_decltype(25).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_timestamp_is_text() {
        assert_eq!(oid_to_sqlite_decltype(1114).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_timestamptz_is_text() {
        assert_eq!(oid_to_sqlite_decltype(1184).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_date_is_text() {
        assert_eq!(oid_to_sqlite_decltype(1082).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_time_is_text() {
        assert_eq!(oid_to_sqlite_decltype(1083).to_str().unwrap(), "TEXT");
    }

    #[test]
    fn decltype_unknown_oid_is_text() {
        assert_eq!(oid_to_sqlite_decltype(9999).to_str().unwrap(), "TEXT");
    }

    // ── convert_metadata_settings_upsert ─────────────────────────────────────

    #[test]
    fn upsert_non_matching_sql_returns_none() {
        assert_eq!(
            convert_metadata_settings_upsert("SELECT * FROM some_table"),
            None
        );
    }

    #[test]
    fn upsert_insert_without_table_returns_none() {
        assert_eq!(
            convert_metadata_settings_upsert("INSERT INTO other_table VALUES (1)"),
            None
        );
    }

    #[test]
    fn upsert_qualifying_insert_returns_upsert_sql() {
        let sql = "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x')";
        let result = convert_metadata_settings_upsert(sql);
        assert!(result.is_some());
        let upsert = result.unwrap();
        assert!(upsert.starts_with(sql));
        assert!(upsert.contains("ON CONFLICT (account_id, guid)"));
        assert!(upsert.contains("DO UPDATE SET"));
        assert!(upsert.contains("RETURNING id"));
    }

    #[test]
    fn upsert_already_has_on_conflict_returns_none() {
        let sql = "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x') \
                   ON CONFLICT (account_id, guid) DO NOTHING";
        assert_eq!(convert_metadata_settings_upsert(sql), None);
    }

    #[test]
    fn upsert_already_has_returning_returns_none() {
        let sql = "INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x') \
                   RETURNING id";
        assert_eq!(convert_metadata_settings_upsert(sql), None);
    }

    #[test]
    fn upsert_empty_string_returns_none() {
        assert_eq!(convert_metadata_settings_upsert(""), None);
    }

    #[test]
    fn upsert_case_insensitive_match() {
        let sql = "insert into METADATA_ITEM_SETTINGS (account_id, guid) values (1, 'x')";
        let result = convert_metadata_settings_upsert(sql);
        assert!(result.is_some());
        assert!(result.unwrap().contains("ON CONFLICT"));
    }

    // FFI variants

    #[test]
    fn upsert_ffi_null_returns_null() {
        let ptr = rust_convert_metadata_settings_upsert(std::ptr::null());
        assert!(ptr.is_null());
    }

    #[test]
    fn upsert_ffi_non_matching_returns_null() {
        let input = cs("SELECT 1");
        let ptr = rust_convert_metadata_settings_upsert(input.as_ptr());
        assert!(ptr.is_null());
    }

    #[test]
    fn upsert_ffi_qualifying_returns_non_null_and_must_free() {
        let input =
            cs("INSERT INTO plex.metadata_item_settings (account_id, guid) VALUES (1, 'x')");
        let ptr = rust_convert_metadata_settings_upsert(input.as_ptr());
        assert!(!ptr.is_null());
        // Safety: ptr was returned by into_raw() and we must free it.
        let result = unsafe { CString::from_raw(ptr) };
        let s = result.to_str().unwrap();
        assert!(s.contains("ON CONFLICT"));
    }

    // ── extract_metadata_id ───────────────────────────────────────────────────

    #[test]
    fn extract_url_encoded_pattern_returns_id() {
        let sql =
            "INSERT INTO play_queue_generators (uri) VALUES ('server://x%2Fmetadata%2F12345%2F')";
        assert_eq!(extract_metadata_id(sql), 12345);
    }

    #[test]
    fn extract_plain_slash_pattern_returns_id() {
        let sql =
            "INSERT INTO play_queue_generators (uri) VALUES ('server://x/metadata/67890/other')";
        assert_eq!(extract_metadata_id(sql), 67890);
    }

    #[test]
    fn extract_not_a_play_queue_insert_returns_zero() {
        let sql = "INSERT INTO some_other_table (uri) VALUES ('/metadata/999')";
        assert_eq!(extract_metadata_id(sql), 0);
    }

    #[test]
    fn extract_no_metadata_pattern_returns_zero() {
        let sql = "INSERT INTO play_queue_generators (uri) VALUES ('something-else')";
        assert_eq!(extract_metadata_id(sql), 0);
    }

    #[test]
    fn extract_empty_string_returns_zero() {
        assert_eq!(extract_metadata_id(""), 0);
    }

    #[test]
    fn extract_not_an_insert_returns_zero() {
        let sql = "SELECT * FROM play_queue_generators WHERE uri LIKE '%/metadata/1%'";
        assert_eq!(extract_metadata_id(sql), 0);
    }

    #[test]
    fn extract_single_digit_id() {
        let sql = "INSERT INTO play_queue_generators (uri) VALUES ('/metadata/7')";
        assert_eq!(extract_metadata_id(sql), 7);
    }

    #[test]
    fn extract_large_id() {
        let sql = "INSERT INTO play_queue_generators (uri) VALUES ('/metadata/9876543210')";
        assert_eq!(extract_metadata_id(sql), 9_876_543_210);
    }

    // FFI variants

    #[test]
    fn extract_ffi_null_returns_zero() {
        assert_eq!(rust_extract_metadata_id(std::ptr::null()), 0);
    }

    #[test]
    fn extract_ffi_url_encoded_returns_id() {
        let input = cs("INSERT INTO play_queue_generators (uri) VALUES ('x%2Fmetadata%2F42')");
        assert_eq!(rust_extract_metadata_id(input.as_ptr()), 42);
    }

    #[test]
    fn extract_ffi_non_matching_returns_zero() {
        let input = cs("INSERT INTO other_table VALUES (1)");
        assert_eq!(rust_extract_metadata_id(input.as_ptr()), 0);
    }
}
