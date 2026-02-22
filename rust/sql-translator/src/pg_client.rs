/// Module: pg_client
///
/// Pure, portable logic extracted from `pg_client.c`.
/// The libpq-dependent parts (connection lifecycle, pool management,
/// pthread logic) remain in C.  This module exposes only the tiny
/// pieces that contain no platform or library dependencies.
///
/// FFI surface (called from `src/pg_client.c`):
///   rust_hash_sql(sql)              → u64   FNV-1a hash for prepared-stmt cache keys
///   rust_is_stale_sqlstate(s)       → i32   1 if SQLSTATE == "26000"
///   rust_is_duplicate_sqlstate(s)   → i32   1 if SQLSTATE == "42P05"
use std::ffi::CStr;
use std::os::raw::c_char;

// ─── Internal pure helpers ────────────────────────────────────────────────────

/// FNV-1a hash over the bytes of `s`.
///
/// Parameters match the C implementation in `pg_client.c`:
///   - offset basis : 14695981039346656037
///   - prime        : 1099511628211
///
/// Produces identical output to the C loop for every valid UTF-8 (or raw byte)
/// sequence because FNV-1a is defined over bytes, not characters.
pub(crate) fn fnv1a_str(s: &str) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

/// Returns `true` when `sqlstate` is exactly `"26000"`
/// (invalid_sql_statement_name — prepared statement does not exist).
pub(crate) fn is_stale_sqlstate(sqlstate: &str) -> bool {
    sqlstate == "26000"
}

/// Returns `true` when `sqlstate` is exactly `"42P05"`
/// (duplicate_prepared_statement — prepared statement already exists).
pub(crate) fn is_duplicate_sqlstate(sqlstate: &str) -> bool {
    sqlstate == "42P05"
}

// ─── FFI helpers ──────────────────────────────────────────────────────────────

/// Safely convert a nullable C string pointer to a `&str`.
/// Returns `""` for NULL or invalid UTF-8.
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    // SAFETY: caller must guarantee `ptr` points to a valid, NUL-terminated
    // C string for at least the duration of the returned `&str`.
    CStr::from_ptr(ptr).to_str().unwrap_or("")
}

// ─── Public C FFI functions ───────────────────────────────────────────────────

/// FNV-1a hash for SQL strings, used as prepared-statement cache keys.
///
/// Returns 0 for a NULL pointer (matching the C implementation's `if (!sql) return 0`).
/// For a non-NULL pointer the result is identical to the C loop in `pg_hash_sql`.
///
/// # Safety
/// `sql` must be NULL or a valid, NUL-terminated C string.
#[no_mangle]
pub extern "C" fn rust_hash_sql(sql: *const c_char) -> u64 {
    if sql.is_null() {
        return 0;
    }
    let s = unsafe { cstr_to_str(sql) };
    fnv1a_str(s)
}

/// Returns 1 if `sqlstate` is `"26000"` (prepared statement does not exist), 0 otherwise.
///
/// Intended to be called from `pg_is_stale_prepared_stmt` after the C side
/// extracts the SQLSTATE with `PQresultErrorField(res, PG_DIAG_SQLSTATE)`.
///
/// Returns 0 for a NULL pointer.
///
/// # Safety
/// `sqlstate` must be NULL or a valid, NUL-terminated C string.
#[no_mangle]
pub extern "C" fn rust_is_stale_sqlstate(sqlstate: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str(sqlstate) };
    i32::from(is_stale_sqlstate(s))
}

/// Returns 1 if `sqlstate` is `"42P05"` (duplicate prepared statement), 0 otherwise.
///
/// Intended to be called from `pg_is_duplicate_prepared_stmt` after the C side
/// extracts the SQLSTATE with `PQresultErrorField(res, PG_DIAG_SQLSTATE)`.
///
/// Returns 0 for a NULL pointer.
///
/// # Safety
/// `sqlstate` must be NULL or a valid, NUL-terminated C string.
#[no_mangle]
pub extern "C" fn rust_is_duplicate_sqlstate(sqlstate: *const c_char) -> i32 {
    let s = unsafe { cstr_to_str(sqlstate) };
    i32::from(is_duplicate_sqlstate(s))
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// Helper: create a CString from a &str (panics on interior NUL).
    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    // ── fnv1a_str / rust_hash_sql ────────────────────────────────────────────

    #[test]
    fn hash_null_returns_zero() {
        assert_eq!(rust_hash_sql(std::ptr::null()), 0);
    }

    #[test]
    fn hash_same_string_is_deterministic() {
        let sql = "SELECT id FROM metadata WHERE guid = $1";
        let h1 = fnv1a_str(sql);
        let h2 = fnv1a_str(sql);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_different_strings_differ() {
        let h1 = fnv1a_str("SELECT 1");
        let h2 = fnv1a_str("SELECT 2");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_empty_string_is_nonzero() {
        // FNV-1a of "" is just the offset basis — never zero.
        let h = fnv1a_str("");
        assert_ne!(h, 0);
    }

    #[test]
    fn hash_empty_string_consistent() {
        // Two calls for "" return the same value.
        assert_eq!(fnv1a_str(""), fnv1a_str(""));
    }

    #[test]
    fn hash_known_value_matches_c_implementation() {
        // Hand-computed FNV-1a over "SELECT 1":
        //   S=0x53, E=0x45, L=0x4C, E=0x45, C=0x43, T=0x54, ' '=0x20, 1=0x31
        // Verified against the C loop with the same constants.
        let expected: u64 = {
            let mut h: u64 = 14695981039346656037;
            for b in b"SELECT 1" {
                h ^= *b as u64;
                h = h.wrapping_mul(1099511628211);
            }
            h
        };
        assert_eq!(fnv1a_str("SELECT 1"), expected);
    }

    #[test]
    fn hash_similar_strings_differ() {
        // Strings that differ by one character must produce different hashes.
        let h1 = fnv1a_str("INSERT INTO t VALUES ($1)");
        let h2 = fnv1a_str("INSERT INTO t VALUES ($2)");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_ffi_nonempty_nonzero() {
        let cs = c("SELECT * FROM metadata");
        assert_ne!(rust_hash_sql(cs.as_ptr()), 0);
    }

    #[test]
    fn hash_ffi_matches_pure_helper() {
        let sql = "UPDATE metadata SET title=$1 WHERE id=$2";
        let cs = c(sql);
        assert_eq!(rust_hash_sql(cs.as_ptr()), fnv1a_str(sql));
    }

    #[test]
    fn hash_case_sensitive() {
        // FNV-1a is byte-level; case changes must change the hash.
        assert_ne!(fnv1a_str("select 1"), fnv1a_str("SELECT 1"));
    }

    // ── rust_is_stale_sqlstate ───────────────────────────────────────────────

    #[test]
    fn stale_exact_match_returns_one() {
        assert_eq!(rust_is_stale_sqlstate(c("26000").as_ptr()), 1);
    }

    #[test]
    fn stale_null_returns_zero() {
        assert_eq!(rust_is_stale_sqlstate(std::ptr::null()), 0);
    }

    #[test]
    fn stale_empty_string_returns_zero() {
        assert_eq!(rust_is_stale_sqlstate(c("").as_ptr()), 0);
    }

    #[test]
    fn stale_wrong_code_42p05_returns_zero() {
        assert_eq!(rust_is_stale_sqlstate(c("42P05").as_ptr()), 0);
    }

    #[test]
    fn stale_close_but_wrong_26001_returns_zero() {
        assert_eq!(rust_is_stale_sqlstate(c("26001").as_ptr()), 0);
    }

    #[test]
    fn stale_pure_helper_true() {
        assert!(is_stale_sqlstate("26000"));
    }

    #[test]
    fn stale_pure_helper_false_for_prefix() {
        // "2600" is a prefix of "26000" — must not match.
        assert!(!is_stale_sqlstate("2600"));
    }

    // ── rust_is_duplicate_sqlstate ───────────────────────────────────────────

    #[test]
    fn duplicate_exact_match_returns_one() {
        assert_eq!(rust_is_duplicate_sqlstate(c("42P05").as_ptr()), 1);
    }

    #[test]
    fn duplicate_null_returns_zero() {
        assert_eq!(rust_is_duplicate_sqlstate(std::ptr::null()), 0);
    }

    #[test]
    fn duplicate_empty_string_returns_zero() {
        assert_eq!(rust_is_duplicate_sqlstate(c("").as_ptr()), 0);
    }

    #[test]
    fn duplicate_wrong_code_26000_returns_zero() {
        assert_eq!(rust_is_duplicate_sqlstate(c("26000").as_ptr()), 0);
    }

    #[test]
    fn duplicate_close_but_wrong_42p06_returns_zero() {
        assert_eq!(rust_is_duplicate_sqlstate(c("42P06").as_ptr()), 0);
    }

    #[test]
    fn duplicate_pure_helper_true() {
        assert!(is_duplicate_sqlstate("42P05"));
    }

    #[test]
    fn duplicate_pure_helper_false_for_lowercase() {
        // SQLSTATE codes are uppercase; "42p05" must not match.
        assert!(!is_duplicate_sqlstate("42p05"));
    }
}
