# Release Notes - v0.9.36

**Release Date:** February 21, 2026

Pool auto-grow: the connection pool now grows automatically when Plex creates more threads than the configured pool size, preventing permanent thread lockout (Issue #9).

## Highlights

### Pool Auto-Grow (Issue #9)

- **Problem:** When `PLEX_PG_POOL_SIZE` is set below the number of concurrent Plex threads (e.g., during library scan + streaming), excess threads are permanently locked out and metadata writes fail.
- **Fix:** The connection pool now auto-grows on demand. When all slots are occupied and a new thread needs a connection, the pool grows by one slot (up to the maximum of 200). The existing reaper closes idle connections when demand drops.
- Pool auto-grows from configured size up to `POOL_SIZE_MAX` (200)
- Startup warning when pool size is below recommended minimum (80)
- Atomic pool size for lock-free growth via CAS
- Verified live: pool=20, 10 simultaneous library scans → grew to 51, 0 errors, connections reaped after idle

### Stress Tests

New test suite for pool behavior under load:
- `test_pool_autogrow`: verifies grow/shrink/re-grow cycle
- `test_stress_load`: direct libpq stress test (20 threads, 30s)
- `test_pool_exhaustion`: proves pool exhaustion with fixed size
- `test_pool_modes`: compares thread-affinity, borrow, and idle pool strategies

## Upgrading

No action required. Default pool size remains 150. Users who never set `PLEX_PG_POOL_SIZE` are unaffected. Users with a low `PLEX_PG_POOL_SIZE` value will benefit automatically — the pool grows as needed instead of locking out threads.
