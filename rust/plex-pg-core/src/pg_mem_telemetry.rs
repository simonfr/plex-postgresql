/// Module: pg_mem_telemetry
///
/// Memory telemetry backend for the plex PostgreSQL redirect layer.
/// Tracks per-subsystem byte and event counts atomically, and emits a compact
/// summary line to the shim log every 60 seconds (ERROR level, always visible).
///
/// Enabled only when `PLEX_PG_MEM_TELEMETRY=1`.
///
/// FFI surface:
///   rust_mem_telemetry_enabled()  -> i32
///   rust_mem_telemetry_add(counter: i32, bytes: u64, events: u64)
///   rust_mem_telemetry_maybe_log()
use std::ffi::CString;
use std::io::Write;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Counter definitions ──────────────────────────────────────────────────────

/// Number of tracked memory counter types.
pub const PMT_COUNTER_MAX: usize = 7;

/// Human-readable names for each counter, matching the C enum order.
pub const COUNTER_NAMES: [&str; PMT_COUNTER_MAX] = [
    "bind_text",
    "bind_hex",
    "bind_val_blob",
    "col_cached_blob",
    "col_decoded_blob",
    "bind_replace_free",
    "stmt_sweep_free",
];

// ─── Global atomic state ──────────────────────────────────────────────────────

/// Accumulated byte totals per counter (monotonically increasing).
static G_BYTES: [AtomicU64; PMT_COUNTER_MAX] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Accumulated event totals per counter (monotonically increasing).
static G_EVENTS: [AtomicU64; PMT_COUNTER_MAX] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Cached "enabled" flag: -1 = not yet checked, 0 = disabled, 1 = enabled.
static G_ENABLED: AtomicI32 = AtomicI32::new(-1);

/// Unix timestamp (seconds) of the last telemetry log emission.
static G_LAST_LOG_TS: AtomicU64 = AtomicU64::new(0);

// ─── Previous-snapshot state (single-writer; guarded by the CAS on G_LAST_LOG_TS) ──

/// Previous byte snapshot for delta computation. Only written while holding
/// the implicit "log slot" obtained by the CAS in `maybe_log_inner`.
static G_PREV_BYTES: [AtomicU64; PMT_COUNTER_MAX] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Previous event snapshot for delta computation.
static G_PREV_EVENTS: [AtomicU64; PMT_COUNTER_MAX] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Return the current time as seconds since the Unix epoch.
/// Used for the 60-second log interval check, matching the C use of
/// `clock_gettime(CLOCK_MONOTONIC)` — SystemTime is an acceptable proxy for
/// coarse 60-second intervals.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Core of `enabled`: check the `PLEX_PG_MEM_TELEMETRY` env var and cache
/// the result atomically.  Returns `true` when telemetry is active.
pub fn is_enabled() -> bool {
    let v = G_ENABLED.load(Ordering::Relaxed);
    if v != -1 {
        return v == 1;
    }
    // First call: read env var and cache.
    let result = std::env::var("PLEX_PG_MEM_TELEMETRY")
        .map(|s| s.starts_with('1'))
        .unwrap_or(false);
    let flag: i32 = if result { 1 } else { 0 };
    G_ENABLED.store(flag, Ordering::Relaxed);
    result
}

/// Add `bytes` and `events` to counter `idx` (bounds-checked).
pub fn add_to_counter(idx: usize, bytes: u64, events: u64) {
    if idx >= PMT_COUNTER_MAX {
        return;
    }
    G_BYTES[idx].fetch_add(bytes, Ordering::Relaxed);
    G_EVENTS[idx].fetch_add(events, Ordering::Relaxed);
}

/// Build the compact telemetry log line from current snapshots.
///
/// Format mirrors the C implementation exactly:
/// `MEM_TELEMETRY: bind_text=XKB/Yev(d:ZKB/Wev) ... TOTAL=XKB/Yev`
///
/// `prev_bytes` / `prev_events` are the previous snapshot values used to
/// compute per-counter deltas.  This function does NOT update those snapshots
/// — the caller is responsible for updating `G_PREV_BYTES` / `G_PREV_EVENTS`
/// after calling this.
pub fn build_log_line(
    snap_bytes: &[u64; PMT_COUNTER_MAX],
    snap_events: &[u64; PMT_COUNTER_MAX],
    prev_bytes: &[u64; PMT_COUNTER_MAX],
    prev_events: &[u64; PMT_COUNTER_MAX],
) -> String {
    let mut total_bytes: u64 = 0;
    let mut total_events: u64 = 0;
    let mut line = String::from("MEM_TELEMETRY:");

    for i in 0..PMT_COUNTER_MAX {
        let db = snap_bytes[i].saturating_sub(prev_bytes[i]);
        let de = snap_events[i].saturating_sub(prev_events[i]);
        total_bytes += snap_bytes[i];
        total_events += snap_events[i];
        if de > 0 {
            line.push_str(&format!(
                " {}={}KB/{}ev(d:{}KB/{}ev)",
                COUNTER_NAMES[i],
                snap_bytes[i] / 1024,
                snap_events[i],
                db / 1024,
                de
            ));
        }
    }

    line.push_str(&format!(
        " TOTAL={}KB/{}ev",
        total_bytes / 1024,
        total_events
    ));
    line
}

/// Inner implementation of `maybe_log`: attempt to claim the log slot via CAS
/// and emit a telemetry line if 60 seconds have elapsed.
///
/// Separated from the FFI wrapper so it can be exercised by unit tests without
/// requiring a live logging subsystem.  Returns `Some(log_line)` when a line
/// was produced, `None` otherwise (disabled, too early, or lost the CAS race).
pub fn maybe_log_inner(now: u64) -> Option<String> {
    if !is_enabled() {
        return None;
    }

    let prev = G_LAST_LOG_TS.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < 60 {
        return None;
    }

    // Attempt to claim this log slot — only one thread proceeds.
    if G_LAST_LOG_TS
        .compare_exchange(prev, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return None;
    }

    // Collect current snapshots.
    let mut snap_bytes = [0u64; PMT_COUNTER_MAX];
    let mut snap_events = [0u64; PMT_COUNTER_MAX];
    for i in 0..PMT_COUNTER_MAX {
        snap_bytes[i] = G_BYTES[i].load(Ordering::Relaxed);
        snap_events[i] = G_EVENTS[i].load(Ordering::Relaxed);
    }

    // Load previous snapshots.
    let mut prev_bytes = [0u64; PMT_COUNTER_MAX];
    let mut prev_events = [0u64; PMT_COUNTER_MAX];
    for i in 0..PMT_COUNTER_MAX {
        prev_bytes[i] = G_PREV_BYTES[i].load(Ordering::Relaxed);
        prev_events[i] = G_PREV_EVENTS[i].load(Ordering::Relaxed);
    }

    // Build the log line.
    let line = build_log_line(&snap_bytes, &snap_events, &prev_bytes, &prev_events);

    // Update previous snapshots (single writer — we hold the CAS slot).
    for i in 0..PMT_COUNTER_MAX {
        G_PREV_BYTES[i].store(snap_bytes[i], Ordering::Relaxed);
        G_PREV_EVENTS[i].store(snap_events[i], Ordering::Relaxed);
    }

    Some(line)
}

// ─── FFI functions ────────────────────────────────────────────────────────────

/// Returns 1 if memory telemetry is enabled (`PLEX_PG_MEM_TELEMETRY=1`), 0 otherwise.
///
/// The result is cached atomically after the first call.
#[no_mangle]
pub extern "C" fn rust_mem_telemetry_enabled() -> i32 {
    i32::from(is_enabled())
}

/// Atomically add `bytes` and `events` to the given counter slot.
///
/// `counter` must be in the range `[0, PMT_COUNTER_MAX)`.  Out-of-range values
/// are silently ignored (matches the C implementation).
#[no_mangle]
pub extern "C" fn rust_mem_telemetry_add(counter: i32, bytes: u64, events: u64) {
    if counter < 0 {
        return;
    }
    add_to_counter(counter as usize, bytes, events);
}

/// If telemetry is enabled and 60 seconds have elapsed since the last emission,
/// build and write a compact summary line to the shim log at ERROR level.
///
/// Thread-safe: uses a CAS on the timestamp to ensure at most one thread logs
/// per interval.
#[no_mangle]
pub extern "C" fn rust_mem_telemetry_maybe_log() {
    let now = now_secs();
    if let Some(line) = maybe_log_inner(now) {
        // Write via the Rust logging FFI (level 0 = ERROR, always visible).
        match CString::new(line) {
            Ok(cstr) => {
                // Safety: rust_logging_write is defined in pg_logging.rs and
                // is always linked into the same binary.
                extern "C" {
                    fn rust_logging_write(level: i32, message: *const c_char);
                }
                unsafe { rust_logging_write(0, cstr.as_ptr()) };
            }
            Err(_) => {
                // Fallback: if the line somehow contains a null byte (it never
                // should), write a truncated version to stderr and continue.
                let _ = writeln!(
                    std::io::stderr(),
                    "pg_mem_telemetry: failed to build CString for log line"
                );
            }
        }
    }
}

// ─── C ABI wrappers (pg_mem_telemetry.c replacement) ─────────────────────────

#[no_mangle]
pub extern "C" fn pg_mem_telemetry_enabled() -> i32 {
    rust_mem_telemetry_enabled()
}

#[no_mangle]
pub extern "C" fn pg_mem_telemetry_add(counter: i32, bytes: u64, events: u64) {
    rust_mem_telemetry_add(counter, bytes, events);
}

#[no_mangle]
pub extern "C" fn pg_mem_telemetry_maybe_log() {
    rust_mem_telemetry_maybe_log();
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Counter names ────────────────────────────────────────────────────────

    #[test]
    fn counter_names_count_matches_max() {
        assert_eq!(COUNTER_NAMES.len(), PMT_COUNTER_MAX);
    }

    #[test]
    fn counter_name_bind_text() {
        assert_eq!(COUNTER_NAMES[0], "bind_text");
    }

    #[test]
    fn counter_name_bind_hex() {
        assert_eq!(COUNTER_NAMES[1], "bind_hex");
    }

    #[test]
    fn counter_name_bind_val_blob() {
        assert_eq!(COUNTER_NAMES[2], "bind_val_blob");
    }

    #[test]
    fn counter_name_col_cached_blob() {
        assert_eq!(COUNTER_NAMES[3], "col_cached_blob");
    }

    #[test]
    fn counter_name_col_decoded_blob() {
        assert_eq!(COUNTER_NAMES[4], "col_decoded_blob");
    }

    #[test]
    fn counter_name_bind_replace_free() {
        assert_eq!(COUNTER_NAMES[5], "bind_replace_free");
    }

    #[test]
    fn counter_name_stmt_sweep_free() {
        assert_eq!(COUNTER_NAMES[6], "stmt_sweep_free");
    }

    // ── add_to_counter bounds checking ───────────────────────────────────────

    #[test]
    fn add_to_counter_out_of_range_is_ignored() {
        // Should not panic; just a no-op.
        add_to_counter(PMT_COUNTER_MAX, 999, 999);
        add_to_counter(usize::MAX, 999, 999);
    }

    // ── is_enabled (env var caching) ─────────────────────────────────────────

    /// Reset the cached enabled flag so env-var tests start from a known state.
    fn reset_enabled() {
        G_ENABLED.store(-1, Ordering::Relaxed);
    }

    #[test]
    fn enabled_returns_false_when_env_not_set() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");

        assert!(!is_enabled());

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
    }

    #[test]
    fn enabled_returns_true_when_env_is_1() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::set_var("PLEX_PG_MEM_TELEMETRY", "1");

        assert!(is_enabled());

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
    }

    #[test]
    fn enabled_returns_false_when_env_is_0() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::set_var("PLEX_PG_MEM_TELEMETRY", "0");

        assert!(!is_enabled());

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
    }

    #[test]
    fn enabled_returns_false_when_env_is_arbitrary_string() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::set_var("PLEX_PG_MEM_TELEMETRY", "yes");

        assert!(!is_enabled());

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
    }

    #[test]
    fn ffi_enabled_returns_0_when_disabled() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");

        assert_eq!(rust_mem_telemetry_enabled(), 0);

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
    }

    #[test]
    fn ffi_enabled_returns_1_when_enabled() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::set_var("PLEX_PG_MEM_TELEMETRY", "1");

        assert_eq!(rust_mem_telemetry_enabled(), 1);

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
    }

    // ── build_log_line format ────────────────────────────────────────────────

    #[test]
    fn log_line_starts_with_prefix() {
        let zeros = [0u64; PMT_COUNTER_MAX];
        let line = build_log_line(&zeros, &zeros, &zeros, &zeros);
        assert!(line.starts_with("MEM_TELEMETRY:"), "got: {}", line);
    }

    #[test]
    fn log_line_contains_total() {
        let zeros = [0u64; PMT_COUNTER_MAX];
        let line = build_log_line(&zeros, &zeros, &zeros, &zeros);
        assert!(line.contains("TOTAL="), "got: {}", line);
    }

    #[test]
    fn log_line_total_zero_when_no_events() {
        let zeros = [0u64; PMT_COUNTER_MAX];
        let line = build_log_line(&zeros, &zeros, &zeros, &zeros);
        assert!(line.contains("TOTAL=0KB/0ev"), "got: {}", line);
    }

    #[test]
    fn log_line_omits_counters_with_zero_delta_events() {
        let zeros = [0u64; PMT_COUNTER_MAX];
        // Snapshot equals previous — delta is zero — counter should be absent.
        let snap = [1024u64; PMT_COUNTER_MAX]; // 1 KB each
        let line = build_log_line(&snap, &zeros, &snap, &zeros);
        // No counter entry because de == 0 for every counter.
        assert!(!line.contains("bind_text="), "got: {}", line);
    }

    #[test]
    fn log_line_includes_counter_when_delta_events_nonzero() {
        let zeros = [0u64; PMT_COUNTER_MAX];
        let mut snap_events = [0u64; PMT_COUNTER_MAX];
        let mut snap_bytes = [0u64; PMT_COUNTER_MAX];
        snap_events[0] = 5; // bind_text: 5 new events
        snap_bytes[0] = 2048; // bind_text: 2 KB
        let line = build_log_line(&snap_bytes, &snap_events, &zeros, &zeros);
        assert!(line.contains("bind_text="), "got: {}", line);
        // 2048 / 1024 = 2 KB; 5 events; delta = same (prev is zero)
        assert!(
            line.contains("bind_text=2KB/5ev(d:2KB/5ev)"),
            "got: {}",
            line
        );
    }

    #[test]
    fn log_line_delta_uses_cumulative_difference() {
        // Simulate second log call where some new bytes/events arrived.
        let mut prev_bytes = [0u64; PMT_COUNTER_MAX];
        let mut prev_events = [0u64; PMT_COUNTER_MAX];
        prev_bytes[2] = 4096; // bind_val_blob: was 4 KB
        prev_events[2] = 10; // bind_val_blob: was 10 events

        let mut snap_bytes = [0u64; PMT_COUNTER_MAX];
        let mut snap_events = [0u64; PMT_COUNTER_MAX];
        snap_bytes[2] = 8192; // now 8 KB (delta = 4 KB)
        snap_events[2] = 15; // now 15 events (delta = 5)

        let line = build_log_line(&snap_bytes, &snap_events, &prev_bytes, &prev_events);
        // total=8KB/15ev  delta=4KB/5ev
        assert!(
            line.contains("bind_val_blob=8KB/15ev(d:4KB/5ev)"),
            "got: {}",
            line
        );
    }

    #[test]
    fn log_line_total_sums_all_counters() {
        let mut snap_bytes = [0u64; PMT_COUNTER_MAX];
        let mut snap_events = [0u64; PMT_COUNTER_MAX];
        // Give each counter 1 KB and 1 event.
        for i in 0..PMT_COUNTER_MAX {
            snap_bytes[i] = 1024;
            snap_events[i] = 1;
        }
        let zeros = [0u64; PMT_COUNTER_MAX];
        let line = build_log_line(&snap_bytes, &snap_events, &zeros, &zeros);
        let expected_kb = PMT_COUNTER_MAX as u64; // 7 KB total
        let expected_ev = PMT_COUNTER_MAX as u64; // 7 events total
        assert!(
            line.contains(&format!("TOTAL={}KB/{}ev", expected_kb, expected_ev)),
            "got: {}",
            line
        );
    }

    // ── maybe_log_inner timing / CAS ─────────────────────────────────────────

    /// Reset the last-log timestamp and the enabled flag for timing tests.
    fn reset_timing(ts: u64) {
        G_LAST_LOG_TS.store(ts, Ordering::Relaxed);
    }

    #[test]
    fn maybe_log_inner_returns_none_when_disabled() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        reset_timing(0);

        // now = very large so interval check passes; but disabled => None.
        let result = maybe_log_inner(u64::MAX);
        assert!(result.is_none(), "expected None but got: {:?}", result);

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
    }

    #[test]
    fn maybe_log_inner_returns_none_when_interval_not_elapsed() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::set_var("PLEX_PG_MEM_TELEMETRY", "1");

        let base: u64 = 1_000_000;
        reset_timing(base);

        // Only 30 seconds later — should not log.
        let result = maybe_log_inner(base + 30);
        assert!(result.is_none(), "expected None but got: {:?}", result);

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
        reset_timing(0);
    }

    #[test]
    fn maybe_log_inner_returns_some_after_60_seconds() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::set_var("PLEX_PG_MEM_TELEMETRY", "1");

        let base: u64 = 2_000_000;
        reset_timing(base);

        // Exactly 60 seconds later — should log.
        let result = maybe_log_inner(base + 60);
        assert!(result.is_some(), "expected Some but got None");
        let line = result.unwrap();
        assert!(line.starts_with("MEM_TELEMETRY:"), "got: {}", line);

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
        reset_timing(0);
    }

    #[test]
    fn maybe_log_inner_returns_none_on_second_call_at_same_timestamp() {
        reset_enabled();
        let prev = std::env::var("PLEX_PG_MEM_TELEMETRY").ok();
        std::env::set_var("PLEX_PG_MEM_TELEMETRY", "1");

        let base: u64 = 3_000_000;
        reset_timing(base);
        let now = base + 60;

        // First call claims the slot.
        let r1 = maybe_log_inner(now);
        assert!(r1.is_some());

        // Second call with the same `now`: G_LAST_LOG_TS == now,
        // so now - prev == 0 < 60  → should return None.
        let r2 = maybe_log_inner(now);
        assert!(r2.is_none(), "second call should return None");

        std::env::remove_var("PLEX_PG_MEM_TELEMETRY");
        if let Some(v) = prev {
            std::env::set_var("PLEX_PG_MEM_TELEMETRY", v);
        }
        reset_enabled();
        reset_timing(0);
    }

    // ── FFI add / bounds checking ────────────────────────────────────────────

    #[test]
    fn ffi_add_negative_counter_is_ignored() {
        // Should not panic.
        rust_mem_telemetry_add(-1, 1024, 1);
    }

    #[test]
    fn ffi_add_out_of_range_counter_is_ignored() {
        // PMT_COUNTER_MAX as i32 is one past the end.
        rust_mem_telemetry_add(PMT_COUNTER_MAX as i32, 1024, 1);
    }

    #[test]
    fn ffi_add_valid_counter_updates_bytes_and_events() {
        // Use a fresh snapshot read to verify the add was applied.
        // We use counter 6 (stmt_sweep_free) with a large unique value to
        // avoid interference with other tests running in the same process.
        let before_bytes = G_BYTES[6].load(Ordering::Relaxed);
        let before_events = G_EVENTS[6].load(Ordering::Relaxed);

        rust_mem_telemetry_add(6, 8192, 3);

        let after_bytes = G_BYTES[6].load(Ordering::Relaxed);
        let after_events = G_EVENTS[6].load(Ordering::Relaxed);
        assert_eq!(after_bytes - before_bytes, 8192);
        assert_eq!(after_events - before_events, 3);
    }

    // ── now_secs sanity ──────────────────────────────────────────────────────

    #[test]
    fn now_secs_is_reasonable() {
        let t = now_secs();
        // Must be after 2024-01-01 (Unix 1704067200) and before 2100.
        assert!(t > 1_704_067_200, "time appears to be in the past: {}", t);
        assert!(
            t < 4_102_444_800,
            "time appears to be far in the future: {}",
            t
        );
    }
}
