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
