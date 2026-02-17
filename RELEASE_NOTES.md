# Release Notes - v0.9.32

**Release Date:** February 17, 2026

Shadow SQLite elimination: decltype now matches real SQLite behavior, fixing `std::bad_cast` crashes. Adds camelCase alias quoting and named-parameter dummy prepare.

## Highlights

### Shadow SQLite Elimination Progress

This release continues the work of eliminating the shadow SQLite dependency. The shim is moving towards using PostgreSQL as the sole data source, with SQLite handles serving only as opaque pointers. Key progress in this release:

- **Dummy prepare with named parameters** — PG-routed queries no longer need a real SQLite prepare. A dummy statement (`SELECT 1 WHERE :param1 IS NOT NULL AND ...`) absorbs bind calls using the actual named parameters from the SQL translator result.
- **PQdescribePrepared for column metadata** — `column_count()` and `column_name()` now use PostgreSQL's `PQdescribePrepared` to get column metadata without executing the query, removing another SQLite dependency.
- **Decltype from PostgreSQL catalog** — column type information is resolved from PG's `information_schema.columns` cache instead of relying on the shadow SQLite schema.

### Fixed: `std::bad_cast` in SyncCollections (MetadataCollection.cpp:522)

- **Problem:** Every Plex startup crashed with `std::bad_cast` during the SyncCollections fixup for section 2 (170 collections, 2 tags). The crash occurred during the "Refreshed attributes" phase at `MetadataCollection.cpp:522`.
- **Root cause:** `sqlite3_column_decltype()` returned `"INTEGER"` for aggregate expressions like `count(*)`, `min(year)`, `max(year)`. Real SQLite returns **NULL** for expressions — only actual table columns get a decltype. Plex's SOCI library uses decltype to decide how to create C++ type holders. Non-NULL decltype triggers a "known type" code path with specific `dynamic_cast` expectations; NULL decltype triggers a runtime fallback via `column_type()`. The wrong code path caused the cast to fail.
- **Fix:** Aggregate expressions (`count`, `sum`, `min(`, `max(`, `avg(`) now return NULL from `column_decltype()`, matching real SQLite behavior. SOCI correctly falls back to `column_type()` (SQLITE_INTEGER) for runtime type detection.
- **Impact:** Also fixes "Saving activity history aborted with DB exception" and "ViewStateSync exception" — both had the same root cause.

### camelCase Alias Quoting

- **Problem:** PostgreSQL lowercases unquoted identifiers, breaking Plex queries that use camelCase aliases like `blankKeyTaggingId`, `nonblankKeyId`, `grandparentsSettings`.
- **Fix:** Two-pass approach in `sql_tr_quotes.c`: Pass 1 scans for `AS <mixedCaseIdentifier>` patterns (excluding SQL type keywords like INTEGER, TEXT). Pass 2 quotes all bare occurrences of collected identifiers throughout the query.

### Logging Fork fd Leak Fix

- **Problem:** Child processes forked by Plex inherited the shim's log fd. When the child exited, the destructor ran `fclose()` on the shared fd, silently killing logging in the parent Plex process.
- **Fix:** Store `logging_owner_pid` at init; only `fclose()` in the destructor if `getpid() == logging_owner_pid`.

### Log Noise Reduction

- "STREAM: zero rows returned" downgraded from ERROR to DEBUG
- "RESOLVE_TABLES: Using alternate connection" downgraded from ERROR to DEBUG
- "pg_stmt_clear_result: drained N results after cancel" downgraded from ERROR to DEBUG

## Testing

261 unit tests (220 SQL translator + 41 shadow elimination), 0 failures. 11 new tests for camelCase alias quoting.

## Upgrade Notes

1. Re-run `scripts/install_wrappers.sh` (macOS) or restart the service (Linux/Docker) after updating.
2. No database changes required.

## Files Changed

- `src/db_interpose_column.c` — Aggregate decltype returns NULL (not "INTEGER"); PQdescribePrepared metadata; decltype cache rewrite; log noise reduction
- `src/db_interpose_prepare.c` — Dummy prepare with named parameters; blobs.db exclusion
- `src/db_interpose_step.c` — Log noise reduction
- `src/pg_statement.c` — Log noise reduction
- `src/pg_logging.c` — Fork fd leak fix (logging_owner_pid)
- `src/pg_types.h` — col_names/num_col_names on pg_stmt_t
- `src/pg_client.c` — Transaction commit guards
- `src/sql_tr_quotes.c` — quote_mixed_case_identifiers() two-pass, is_sql_type_keyword()
- `src/sql_translator.c` — Step 10: camelCase alias quoting
- `src/sql_translator_internal.h` — quote_mixed_case_identifiers() declaration
- `src/db_interpose_common.c` — is_blobs_db_path(), rewrite_blobs_schema_migrations()
- `src/db_interpose_exec.c` — blobs_rewrite call
- `src/db_interpose_open.c` — Dead code removed
- `tests/src/test_sql_translator.c` — 11 new camelCase quoting tests
- `VERSION`, `CHANGELOG.md`, `README.md`, `README.es.md`, `RELEASE_NOTES.md`

---

# Release Notes - v0.9.31

**Release Date:** February 16, 2026

Docker build fix: added missing source files and enforced LF line endings for Windows compatibility.

## Highlights

### Docker Build Fix

- **Problem:** `Dockerfile` and `build_shim_musl.sh` were missing 5 source files added in v0.9.27–v0.9.30 (`db_interpose_value.c`, `db_interpose_common.c`, `platform_backtrace.c`, `pg_mem_telemetry.c`, `shim_alloc.c`). The shim compiled but failed at runtime with `symbol not found` errors.
- **Fix:** Added all missing source files to both build scripts. Verified Docker container starts cleanly.

### Windows CRLF Fix (Issue #6)

- **Problem:** Cloning on Windows converted LF→CRLF, breaking shell script shebangs (`#!/usr/bin/with-contenv bash\r`) inside Docker containers.
- **Fix:** Added `.gitattributes` to enforce LF line endings for all scripts, Dockerfiles, and source files.

## Files Changed

- `Dockerfile` — Added 5 missing source files to gcc compile step
- `build_shim_musl.sh` — Added 5 missing source files to compile and link steps
- `.gitattributes` — New: enforces LF line endings across platforms

---

# Release Notes - v0.9.30

**Release Date:** February 16, 2026

Opt-in shim memory tracker with leak trace for production diagnostics.

## Highlights

### Memory Allocation Tracker

- `PLEX_PG_ALLOC_TRACK=1` — logs live/peak/alloc/free summary every 60s
- `PLEX_PG_ALLOC_TRACE=1` — also logs top 15 unfreed allocation sites with file:line
- Disabled by default, zero overhead when off
- 65K-entry hash table for accurate size tracking at free()
- Verified: ~3.4MB live, ~3.5MB peak under normal Plex load

## Files Changed

- `src/shim_alloc.h`, `src/shim_alloc.c` — New allocation tracker module
- 15 source files — Added `#include "shim_alloc.h"` for macro-based tracking
- `Makefile` — Added `shim_alloc.o` to build and test targets
- `README.md` — Memory Tracking section

---

# Release Notes - v0.9.29

**Release Date:** February 16, 2026

Fixed streaming connection corruption that caused Plex to re-run all migrations and crash with `std::bad_cast`. Added single-row streaming for all READ queries.

## Highlights

### Connection Isolation Fix (v0.9.29)

- **Problem:** `resolve_column_tables()` and `preload_decltype_cache()` called `PQexec()` on the streaming connection, consuming pending results. The next `PQgetResult` returned NULL, making Plex think no migrations had run, triggering a full re-migration and `std::bad_cast` crash.
- **Fix:** Both functions now acquire an alternate pool connection when the passed connection has `streaming_active=1`. Pool acquisition (fast path + PHASE 1) also skips streaming connections. `PQcancel` is called before all 6 drain loops.
- **Result:** Plex starts cleanly, continueWatching endpoint returns full JSON.

### Single-Row Streaming Mode (v0.9.28)

- **Feature:** All READ queries now use PostgreSQL's `PQsetSingleRowMode` to stream results row by row instead of loading entire result sets into memory.
- **Fallback:** If `PQsetSingleRowMode` fails, automatic fallback to eager fetch.

### Dummy Shadow Statement Fallback (v0.9.29)

- **Feature:** When shadow SQLite prepare fails for READ queries, a dummy `SELECT 1 WHERE ? IS NOT NULL AND ...` is built with matching parameter count so `sqlite3_bind_*` calls succeed and the query runs purely on PostgreSQL.

### SQL Translation Fixes (v0.9.28)

- Placeholder counting missed parameters after string literals
- Upsert translation failed when column list was absent
- String quoting edge cases

## Testing

798 unit tests across 24 suites, 0 failures. 37 new tests:
- 17 connection isolation tests (streaming_active lifecycle, pool isolation, resolve/decltype guards, multi-threaded)
- 20 shadow fallback tests (parameter counting, dummy generation, edge cases, end-to-end)

## Upgrade Notes

1. Re-run `scripts/install_wrappers.sh` (macOS) or restart the service (Linux/Docker) after updating.
2. No database changes required.

## Files Changed

- `src/pg_types.h` — Added `streaming_active` flag to `pg_connection_t`
- `src/pg_client.c` — Pool streaming guards (fast path + PHASE 1)
- `src/pg_statement.c` — PQcancel in free/clear_result, streaming_active clear
- `src/db_interpose_step.c` — PQcancel in drain loops, streaming_active set/clear
- `src/db_interpose_column.c` — resolve_column_tables + preload_decltype_cache streaming guards
- `src/db_interpose_prepare.c` — DDL IF NOT EXISTS + dummy shadow fallback
- `tests/src/test_connection_isolation.c` — 17 new tests
- `tests/src/test_shadow_fallback.c` — 20 new tests
- `Makefile` — Added test-isolation, test-shadow targets

---

# Release Notes - v0.9.27

**Release Date:** February 14, 2026

Removed SyncCollections COMPAT workarounds that caused 223 LPE errors per Plex startup.

## Highlights

### SyncCollections COMPAT Skips Removed

- **Problem:** Two SyncCollections query patterns (blank-key cleanup and tag aggregation) were being intercepted and replaced with empty result sets to avoid `std::bad_cast` crashes. This meant Plex had no collection data at startup, causing 223 "Failed to generate a query" LPE errors every time.
- **Fix:** The underlying `std::bad_cast` issues were already fixed in v0.9.23 (`dt_integer(8)` decltype mapping for OID=20 bigint columns, and column alias fixes for `count(*)`/`min(year)`/`max(year)`). The COMPAT skips are now removed, letting the queries execute normally.
- **Result:** 0 LPE errors at startup (was 223). Collections, hubs, and recommendations now populate correctly.

## Testing

738 unit tests, 0 failures. No new tests — this is a removal of workaround code.

## Upgrade Notes

1. Re-run `scripts/install_wrappers.sh` (macOS) or restart the service (Linux/Docker) after updating.
2. No database changes required.

## Files Changed

- `src/db_interpose_prepare.c` — Removed two SyncCollections COMPAT intercept blocks (~20 lines), replaced with comment explaining the fix history
- `VERSION`, `CHANGELOG.md`, `README.md`, `README.es.md`

---

# Release Notes - v0.9.26

**Release Date:** February 13, 2026

Plex v1.43 compatibility: rewrote JSON operator translation, added `instr()` support, and fixed a data-loss bug in CSV-based migration.

## Highlights

### JSON `->>` Operator Rewrite

- **Problem:** The previous LIKE-based workaround for `col ->> '$.key'` consumed bind parameters, causing "bind message supplies N parameters, but prepared statement requires M" errors on Plex v1.43.0.10492 voice-activity-detection queries.
- **Fix:** Clean translation to native PostgreSQL `col::json->>'key'`. Strips `'$.'` prefix, removes whitespace before `::json`, preserves all bind parameters.

### `instr()` Function Translation

- **Problem:** SQLite's `instr(haystack, needle)` not recognized by PostgreSQL, causing "function instr(text, unknown) does not exist" on Last.fm blacklist queries.
- **Fix:** Translated to PostgreSQL's `STRPOS(haystack, needle)`.

### Migration CSV Truncation Fix

- **Problem:** `sqlite3 -csv` export silently truncated TEXT fields larger than ~8KB with embedded quotes. Affected 133 rows in `media_parts.extra_data` on a typical library. The `url` field contains URL-encoded `%22` sequences that confused CSV parsing.
- **Fix:** Replaced CSV pipeline with Python bridge (`scripts/migrate_table.py`) using `COPY FROM STDIN` with tab-delimited data for lossless transfer.

### Truncated JSON Auto-Repair (doctor.sh)

- `doctor.sh` now detects truncated JSON in `extra_data` columns across `media_parts`, `media_items`, `metadata_items`, and `metadata_item_settings`.
- Auto-repairs by trimming the redundant `url` field and closing the JSON object.

### Log Level Cleanup

- All `TRACE_BADCAST` and `TRACE_PREPARE` messages downgraded from `LOG_ERROR` to `LOG_DEBUG` — they are opt-in diagnostic traces, not errors.

### CI Fix

- `test-stmt-free` and `test-bind-mismatch` Makefile targets now skip the macOS `leaks` tool on Linux instead of failing.

## Testing

738 unit tests, 0 failures. 3 new tests added:
- `instr()` translation (2 tests)
- Real Plex VAD query with 3 bind params (1 test)

## Upgrade Notes

1. Re-run `scripts/install_wrappers.sh` (macOS) or restart the service (Linux/Docker) after updating.
2. Run `scripts/doctor.sh` to detect and auto-repair any truncated JSON from prior CSV-based migrations.
3. If migrating fresh from SQLite, the new Python bridge handles data transfer losslessly — no manual steps needed.

## Files Changed

- `src/sql_tr_query.c` — Rewrote `fix_json_operator_on_text()`: LIKE-hack → `::json->>` cast
- `src/sql_tr_functions.c` — Added `translate_instr()` function
- `src/sql_translator.c` — Added `translate_instr` to pipeline (step 5c)
- `src/sql_translator_internal.h` — Added `translate_instr` declaration
- `src/db_interpose_column.c` — `TRACE_BADCAST` messages → `LOG_DEBUG`
- `src/db_interpose_prepare.c` — `TRACE_PREPARE` messages → `LOG_DEBUG`
- `scripts/migrate_lib.sh` — Replaced CSV export with Python bridge + JSON integrity check
- `scripts/migrate_sqlite_to_pg.sh` — Replaced CSV export with Python bridge
- `scripts/migrate_table.py` — **New:** Python bridge for lossless SQLite→PG via COPY
- `scripts/doctor.sh` — Added truncated JSON detection + auto-repair
- `Makefile` — `leaks` gracefully skipped on Linux
- `tests/src/test_sql_translator.c` — 3 new tests, 4 updated JSON tests
- `VERSION`, `CHANGELOG.md`, `README.md`, `README.es.md`

---

# Release Notes - v0.9.16

**Release Date:** February 8, 2026

This release focuses on wrapper reliability, release automation, and consistent zip-based distribution.

## Highlights

### macOS Wrapper Reliability

- Removed hardcoded machine-specific paths from generated server wrapper.
- Wrapper now uses install-time shim placeholders and sane user-home defaults.
- SQLite shadow `schema_migrations` is now synced from PostgreSQL during wrapper init.

### Scanner Backup/Restore Fix

- Installer now preserves `Plex Media Scanner.original` before patching.
- Uninstaller now restores scanner when backup exists.
- If backup is missing, uninstaller prints a clear warning instead of silently claiming full restore.

### CI/CD Improvements

- Added PR/main CI workflow: `.github/workflows/ci.yml`
  - `bash -n` validation for scripts
  - Linux amd64 builder smoke check
- Added tag-driven macOS release workflow: `.github/workflows/release-macos-artifacts.yml`
- Updated Linux release workflow: `.github/workflows/release-linux-artifacts.yml`
  - Packages and uploads Linux zip bundle automatically

### Release Assets

- Zip-only release assets are now the standard format:
  - `plex-postgresql-vX.Y.Z-macos.zip`
  - `plex-postgresql-vX.Y.Z-linux.zip`

## Upgrade Notes

1. Re-run `scripts/install_wrappers.sh` on macOS to refresh wrapper/scanner behavior.
2. If scanner was patched in older versions without a `.original` backup, reinstall Plex once to reset scanner binary baseline.
3. Prefer zip assets from GitHub Releases for installs/upgrades.

## Files Changed (v0.9.16 scope)

- `scripts/install_wrappers.sh`
- `scripts/uninstall_wrappers.sh`
- `.github/workflows/ci.yml`
- `.github/workflows/release-linux-artifacts.yml`
- `.github/workflows/release-macos-artifacts.yml`
- `README.md`
- `CHANGELOG.md`
- `VERSION`

---

# Release Notes - v0.8.13

**Release Date:** January 13, 2026

This release reduces shim log noise by demoting high-frequency diagnostic messages to DEBUG.

## Highlights

### 🧹 Reduced Shim Error Spam

**Problem:**
- Repeated ERROR logs from DECLTYPE/COLUMN_TYPE/COLUMN_TEXT_INTEGER flooded normal logs.

**The Fix:**
- Downgraded high-frequency diagnostic logs to DEBUG.
- Real errors remain at ERROR; enable `PLEX_PG_LOG_LEVEL=DEBUG` for deep tracing.

**Impact:**
- ✅ Normal logs are quiet and readable.
- ✅ Debug detail still available when needed.

## Testing

`make` (builds with existing warnings).

## Upgrade Instructions

Use the same upgrade steps as v0.8.12 below.

## Files Changed

- `src/db_interpose_column.c` - Downgrade noisy logs to DEBUG
- `CHANGELOG.md` - Release entry for 0.8.13
- `RELEASE_NOTES.md` - Release notes update
- `VERSION` - Bumped to 0.8.13

## Known Issues

None identified in this release.

---

## v0.8.12 - January 13, 2026

## Highlights

### 🎯 CRITICAL: Fixed std::bad_cast in TV Shows Endpoint

**Problem:**
- TV shows endpoint (`/library/sections/6/all?type=2`) returned HTTP 500
- Error: `Exception handled: std::bad_cast`
- Crash occurred during MetadataCounterCache rebuild
- Movies worked fine, only TV shows affected

**Root Cause:**
Plex's embedded SOCI library has a bug when parsing BIGINT values from aggregate functions (count, sum, max, min, avg) via `row.get<int64_t>()`:

1. PostgreSQL count() returns BIGINT (OID 20)
2. Shim declares column as "BIGINT" 
3. SOCI uses `column_text()` for ALL integer types (not `column_int64()`)
4. SOCI attempts text-to-int conversion with strict type checking
5. Type validation fails → throws `std::bad_cast`

**The Fix:**
Force aggregate functions to declare as TEXT type instead of BIGINT:
- SOCI accepts TEXT → integer conversion without strict type checking
- Detects aggregate column names (count, sum, max, min, avg)
- Returns decltype "TEXT" instead of "BIGINT"
- Bypasses SOCI's buggy integer type validation

**Impact:**
- ✅ TV shows endpoint now returns HTTP 200 (was 500)
- ✅ MetadataCounterCache rebuilds successfully
- ✅ No more std::bad_cast exceptions
- ✅ 1755 TV shows load correctly
- ✅ Movies continue working (backward compatible)

**Technical Details:**
- Similar to SOCI Issue #1190 (fixed in SOCI 4.1.0, but Plex uses older version)
- Discovered after 8+ hours of deep debugging with LLDB, sample profiler, and source analysis
- Workaround implemented in `db_interpose_column.c` line 1573

### 📊 Improved Type Mappings

- INT8/BIGINT (OID 20): Now correctly maps to "BIGINT" (was "INTEGER")
- Proper 64-bit integer handling for non-aggregate BIGINT columns
- INT2/INT4 remain "INTEGER" for SOCI compatibility

## Testing

Run the integration test to verify the fix:
```bash
./tests/test_aggregate_decltype.sh
```

Expected output:
```
Testing Movies endpoint... ✓ PASS (HTTP 200)
Testing TV shows endpoint... ✓ PASS (HTTP 200)
Checking for std::bad_cast exceptions... ✓ PASS (no exceptions found)
```

## Upgrade Instructions

1. **Stop Plex Media Server:**
   ```bash
   pkill "Plex Media Server"
   ```

2. **Update the shim:**
   ```bash
   git pull
   make clean && make
   ```

3. **Restart Plex with the updated shim:**
   ```bash
   DYLD_INSERT_LIBRARIES="./db_interpose_pg.dylib" \
     "/Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server.original" &
   ```

4. **Verify the fix:**
   ```bash
   curl http://localhost:32400/library/sections/6/all?type=2
   # Should return HTTP 200 (not 500)
   ```

## Files Changed

- `src/db_interpose_column.c` - Aggregate decltype workaround
- `src/pg_statement.c` - BIGINT type mapping fix
- `tests/test_aggregate_decltype.sh` - Integration test
- `tests/test_aggregate_decltype.c` - Unit test stub
- `CHANGELOG.md` - Detailed changelog
- `VERSION` - Bumped to 0.8.12

## Known Issues

None identified in this release.

## Related Issues

- SOCI Issue #1190: Identical bug in SOCI's SQLite backend
- See `supernerdanalyse.md` for full debugging journey

---

## Previous Releases

### v0.8.10 - January 12, 2026

Fixed critical connection mismatch bug causing lastval() to retrieve values from the wrong connection.

## Highlights

### CRITICAL: Connection Mismatch Fix (lastval() on Wrong Connection)

Fixed critical bug where `lastval()` retrieved sequence values from a different connection than the one used for INSERT:

| Before | After |
|--------|-------|
| INSERT uses `pg_get_thread_connection()` (thread-local pool) | INSERT uses `pg_get_thread_connection()` |
| `lastval()` uses `pg_find_connection()` (may return different connection) | `lastval()` uses `pg_get_thread_connection()` (same connection) |
| INSERT on conn A, lastval() on conn B → wrong sequence value | INSERT and lastval() on conn A → correct value |

**Root Cause:**

1. INSERT execution uses `pg_get_thread_connection()` to get a thread-local pooled connection
2. `lastval()` used `pg_find_connection()` which could return a DIFFERENT pooled connection
3. Connection pool has multiple selection phases - between INSERT and lastval(), pool state can change:
   - Another thread releases a slot → Phase 2 finds it first
   - TLS cache gets invalidated (slot stolen/released)
   - Connection gets recycled
   - Pool rebalancing occurs
4. Result: INSERT on connection A succeeds, but `lastval()` queries connection B, returning wrong sequence ID

**Solution:**

Modified three metadata functions to use `pg_get_thread_connection()` for library.db:
- `my_sqlite3_last_insert_rowid()` - Ensures lastval() uses same connection as INSERT
- `my_sqlite3_changes()` - Ensures row count from correct connection
- `my_sqlite3_changes64()` - Consistency across all metadata functions

**Impact:**
- Zero overhead - uses existing O(1) thread-local lookup
- 100% consistency - guarantees same connection for INSERT + lastval()
- Prevents entire class of cross-connection visibility bugs

## Files Changed

- `src/db_interpose_metadata.c` - Fixed connection selection for library.db in lastval(), changes(), changes64()

---

# Release Notes - v0.8.9.6

**Release Date:** January 12, 2026

This release fixes a CRITICAL data loss bug in the connection pool that caused playQueues 500 errors.

## Highlights

### CRITICAL FIX: Uncommitted Transactions Lost on Pool Reuse

Fixed critical bug where uncommitted PostgreSQL transactions were aborted when connections were reused from the pool, causing data to disappear despite successful INSERT operations:

| Before | After |
|--------|-------|
| PQreset() called immediately on pool slot reuse | Check for pending transactions before PQreset() |
| Uncommitted transactions rolled back silently | Transactions committed before slot release |
| lastval() returns ID but data is missing | Both sequence ID and data persisted correctly |
| playQueues returns 500 (404 on GET after POST) | playQueues works correctly |

**Root Cause:**

The bug occurred in the connection pool lifecycle:

1. Thread A executes INSERT in implicit transaction
2. INSERT succeeds, lastval() returns sequence ID
3. Thread A releases connection to pool (slot -> SLOT_FREE)
4. Transaction remains UNCOMMITTED
5. Thread B acquires the same slot in PHASE 2
6. PQreset() is called to clean connection state
7. **PQreset() closes/reopens connection, ABORTING uncommitted transaction**
8. Sequence ID persists (non-transactional) but INSERT data is rolled back
9. API returns success with ID, but subsequent GET returns 404

**The Fix (Defense-in-Depth):**

1. **Primary Fix - `pg_close_pool_for_db()`** (line 1044-1062):
   - Check `PQtransactionStatus()` before releasing slot
   - If `PQTRANS_INTRANS` or `PQTRANS_INERROR`, execute COMMIT
   - Ensures no uncommitted work when slot marked SLOT_FREE

2. **Secondary Fix - `pool_get_connection()` PHASE 2** (line 791-805):
   - Check `PQtransactionStatus()` before PQreset()
   - If transaction pending, COMMIT before reset
   - Defense against any edge case where transaction slipped through

**Why This Bug Was Hard to Detect:**

- Sequences are non-transactional, so lastval() always succeeded
- Connection pool reuse is timing-dependent (race condition)
- Only manifested under concurrent load
- INSERT appeared successful from application's perspective

## Technical Details

### PostgreSQL Transaction Semantics

PostgreSQL uses implicit transactions for each statement when not in explicit BEGIN/COMMIT block. PQreset() closes the connection, which triggers an implicit ROLLBACK of any uncommitted transaction.

### Pool Lifecycle States

```
SLOT_READY -> pg_close_pool_for_db() -> SLOT_FREE
                     |
                     v
              [NEW: Check PQtransactionStatus()]
              [NEW: COMMIT if needed]

SLOT_FREE -> pool_get_connection() PHASE 2 -> SLOT_RESERVED
                     |
                     v
              [NEW: Check PQtransactionStatus()]
              [NEW: COMMIT before PQreset()]
```

## Files Changed

- `src/pg_client.c` - Added transaction commit logic in two locations:
  - `pg_close_pool_for_db()` (line 1044-1062) - Commit before release
  - `pool_get_connection()` PHASE 2 (line 791-805) - Commit before reset

## Test Results

Expected results after this fix:

- `/playQueues` POST - 201 Created (data persisted)
- `/playQueues/{id}` GET - 200 OK (data retrievable)
- `/library/metadata/18618` - 200 OK (continues to work)
- `/library/sections/8/all` - 200 OK (continues to work)

## Upgrade Path

Direct upgrade from any 0.8.x version. No configuration changes required.

**CRITICAL:** This is a critical fix for production environments using the connection pool. Upgrade recommended immediately.
