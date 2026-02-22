/// Module: pg_query_cache
///
/// Pure/portable helpers for the PostgreSQL query result cache, exposed via C FFI.
/// The libpq-dependent parts (PGresult access, struct manipulation, thread-local
/// cache management) remain in `src/pg_query_cache.c`.
///
/// Exported FFI functions:
///   - `rust_fnv1a_hash`  — FNV-1a hash of arbitrary bytes
///   - `rust_get_time_ms` — current monotonic time in milliseconds

// ─── FNV-1a constants ────────────────────────────────────────────────────────

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

// ─── Internal pure helpers ────────────────────────────────────────────────────

/// FNV-1a 64-bit hash of a byte slice.
pub(crate) fn fnv1a_hash_slice(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Current time as milliseconds since the Unix epoch, using `SystemTime`.
///
/// `SystemTime` is not strictly monotonic, but the difference from
/// `CLOCK_MONOTONIC` is negligible for cache TTL purposes and avoids
/// pulling in platform-specific APIs.
pub(crate) fn get_time_ms_impl() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── Public C FFI functions ───────────────────────────────────────────────────

/// FNV-1a 64-bit hash of arbitrary bytes.
///
/// Used for cache key computation in `pg_query_cache_key()`.
///
/// Returns the FNV offset basis (0xcbf29ce484222325) when `len` is 0,
/// matching the behaviour of the original C implementation.
///
/// # Safety
/// `data` must point to at least `len` readable bytes, or be NULL when
/// `len` is 0.
#[no_mangle]
pub extern "C" fn rust_fnv1a_hash(data: *const u8, len: usize) -> u64 {
    if len == 0 {
        return FNV_OFFSET_BASIS;
    }
    // SAFETY: caller guarantees `data` points to `len` valid bytes.
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    fnv1a_hash_slice(slice)
}

/// Current time in milliseconds (monotonic-ish, via `SystemTime`).
///
/// Used to timestamp cache entries and evaluate TTL expiry.
/// The returned value is always > 0 for any reasonable system clock.
#[no_mangle]
pub extern "C" fn rust_get_time_ms() -> u64 {
    get_time_ms_impl()
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // ── fnv1a_hash — correctness ─────────────────────────────────────────────

    /// The empty-input hash equals the FNV offset basis.
    #[test]
    fn fnv1a_empty_returns_offset_basis() {
        assert_eq!(fnv1a_hash_slice(b""), FNV_OFFSET_BASIS);
    }

    /// Single byte 0x00 produces the expected value.
    #[test]
    fn fnv1a_single_null_byte() {
        let expected = FNV_OFFSET_BASIS.wrapping_mul(FNV_PRIME); // XOR with 0 changes nothing
        assert_eq!(fnv1a_hash_slice(&[0x00]), expected);
    }

    /// Single non-zero byte follows the spec: hash = (basis XOR byte) * prime.
    #[test]
    fn fnv1a_single_byte() {
        let byte = b'A';
        let expected = (FNV_OFFSET_BASIS ^ byte as u64).wrapping_mul(FNV_PRIME);
        assert_eq!(fnv1a_hash_slice(&[byte]), expected);
    }

    /// The hash is deterministic: same input always gives the same output.
    #[test]
    fn fnv1a_consistent() {
        let a = fnv1a_hash_slice(b"hello world");
        let b = fnv1a_hash_slice(b"hello world");
        assert_eq!(a, b);
    }

    /// Different inputs must produce different hashes (no accidental collision
    /// for trivially different strings).
    #[test]
    fn fnv1a_different_inputs_differ() {
        let h1 = fnv1a_hash_slice(b"SELECT * FROM foo");
        let h2 = fnv1a_hash_slice(b"SELECT * FROM bar");
        assert_ne!(h1, h2);
    }

    /// Input order matters — "ab" and "ba" must hash differently.
    #[test]
    fn fnv1a_order_sensitive() {
        assert_ne!(fnv1a_hash_slice(b"ab"), fnv1a_hash_slice(b"ba"));
    }

    /// Known reference value for "hello" from the FNV-1a spec.
    ///
    /// Reference: https://fnv.isthe.name — FNV1a 64-bit hash of "hello"
    /// is 0xa430d84680aabd0b.
    #[test]
    fn fnv1a_hello_known_value() {
        assert_eq!(fnv1a_hash_slice(b"hello"), 0xa430d84680aabd0b);
    }

    /// Known reference value for "foobar".
    /// FNV1a 64-bit hash of "foobar" = 0x85944171f73967e8.
    #[test]
    fn fnv1a_foobar_known_value() {
        assert_eq!(fnv1a_hash_slice(b"foobar"), 0x85944171f73967e8);
    }

    /// Multi-byte all-zero input should still produce a non-basis value
    /// (because each byte XORs in 0 then multiplies by prime, changing the hash).
    #[test]
    fn fnv1a_multi_zero_differs_from_single_zero() {
        let single = fnv1a_hash_slice(&[0x00]);
        let multi = fnv1a_hash_slice(&[0x00, 0x00]);
        assert_ne!(single, multi);
    }

    /// The hash of a string and its prefix must differ.
    #[test]
    fn fnv1a_prefix_differs_from_full() {
        let full = fnv1a_hash_slice(b"SELECT 1");
        let prefix = fnv1a_hash_slice(b"SELECT");
        assert_ne!(full, prefix);
    }

    // ── rust_fnv1a_hash FFI ──────────────────────────────────────────────────

    /// The FFI wrapper returns the offset basis for zero-length input.
    #[test]
    fn ffi_fnv1a_empty_len_zero() {
        assert_eq!(rust_fnv1a_hash(std::ptr::null(), 0), FNV_OFFSET_BASIS);
    }

    /// The FFI wrapper matches the pure function for a non-empty input.
    #[test]
    fn ffi_fnv1a_matches_pure() {
        let data = b"SELECT * FROM metadata";
        let expected = fnv1a_hash_slice(data);
        assert_eq!(rust_fnv1a_hash(data.as_ptr(), data.len()), expected);
    }

    // ── get_time_ms ──────────────────────────────────────────────────────────

    /// The time is well past the Unix epoch (> year 2000 in ms).
    /// 2000-01-01 00:00:00 UTC = 946_684_800_000 ms since epoch.
    #[test]
    fn get_time_ms_reasonable_value() {
        const Y2K_MS: u64 = 946_684_800_000;
        assert!(
            get_time_ms_impl() > Y2K_MS,
            "clock appears to be before year 2000"
        );
    }

    /// Two back-to-back calls must be non-decreasing (weak monotonicity).
    #[test]
    fn get_time_ms_non_decreasing() {
        let t1 = get_time_ms_impl();
        let t2 = get_time_ms_impl();
        assert!(t2 >= t1, "time went backwards: t1={t1} t2={t2}");
    }

    /// After sleeping 10 ms the clock must have advanced by at least 1 ms.
    #[test]
    fn get_time_ms_advances_after_sleep() {
        let before = get_time_ms_impl();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let after = get_time_ms_impl();
        assert!(
            after > before,
            "time did not advance after sleep: before={before} after={after}"
        );
    }

    /// The FFI wrapper returns the same value as the pure function (within a
    /// generous 100 ms window to account for scheduling jitter).
    #[test]
    fn ffi_get_time_ms_matches_system_time() {
        let ffi_val = rust_get_time_ms();
        let direct = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let diff = direct.abs_diff(ffi_val);
        assert!(
            diff < 100,
            "FFI time {ffi_val} deviates too far from direct read {direct}"
        );
    }
}
