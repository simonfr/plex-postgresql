# Release Notes - v0.9.34

**Release Date:** February 21, 2026

PostgreSQL restart recovery release: fixes Issue #8 where Plex permanently cached HTTP 500 errors after a PG restart and never recovered, even after PostgreSQL was back up.

## Highlights

### Issue #8: Plex Doesn't Recover After PostgreSQL Restart

- **Problem:** When PostgreSQL restarts (upgrade, crash, maintenance), all ~150 pool connections simultaneously go to `CONNECTION_BAD`. Plex makes queries during the ~2s restart window, gets `SQLITE_ERROR` back from the shim, and **permanently caches these errors**. Endpoints like `/hubs`, `/library/recentlyAdded`, `/library/sections` return HTTP 500 forever — until Plex itself is restarted.
- **Root cause:** Two distinct failure paths leaked errors to Plex:
  1. Threads needing a **new** pool connection got NULL from `pool_get_connection()` (pool exhausted/all errored).
  2. Threads that **already held** a connection saw `CONNECTION_BAD` or `PQsend* failed: no connection to the server` when trying to execute.
- **Fix — two layers:**

  **Layer 1: Pool-level retry** (`pg_client.c` Phase 5)
  
  `pool_get_connection()` now retries with exponential backoff instead of returning NULL:
  - Backoff: 500ms, 1s, 2s, 3s, 4s (5 retries, ~10.5s total)
  - Thread-local counter prevents infinite recursion
  - Covers all threads that need a fresh connection

  **Layer 2: Step-level retry** (`db_interpose_step.c` wrapper)
  
  `my_sqlite3_step()` is now a retry wrapper around the inner `my_sqlite3_step_impl()`:
  - A thread-local flag `step_pg_conn_error` is set to 1 before every `return SQLITE_ERROR` caused by a connection failure in `step_impl`
  - The wrapper checks this flag; if set, it resets statement state via `pg_stmt_clear_result()` and retries with the same backoff schedule
  - Covers threads whose existing connections died mid-query
  - Zero changes to existing error-handling code — all mutex ordering preserved, no deadlock risk
  - Works for both READ (SELECT) and WRITE (INSERT/UPDATE/DELETE) paths

- **Result:** All Plex endpoints (including `/hubs`, `/library/recentlyAdded`, `/library/sections`, `/library/onDeck`, `/library/sections/N/all`) return HTTP 200 within 1-2 seconds after PostgreSQL restarts, with no Plex restart required.

### Blobs UNIQUE Index

- **Problem:** The `blobs` table was missing a `UNIQUE INDEX` on `(linked_type, linked_id, blob_type)`, causing `ON CONFLICT DO UPDATE` upserts to fail with "there is no unique or exclusion constraint matching the ON CONFLICT specification".
- **Fix:** Added `idx_blobs_linked_type_id_blob_type` to `schema/plex_schema.sql` and a check to `scripts/doctor.sh`. Applied to production database.

### Configurable Retry Delays (`PLEX_PG_RETRY_DELAYS`)

The backoff schedule for PostgreSQL reconnection is now configurable:

```bash
# Default (10.5s total): 500ms, 1s, 2s, 3s, 4s
export PLEX_PG_RETRY_DELAYS=500,1000,2000,3000,4000

# Faster recovery for local PG (2.5s total)
export PLEX_PG_RETRY_DELAYS=200,500,1000

# More patient for slow/remote PG (30s total)
export PLEX_PG_RETRY_DELAYS=1000,2000,4000,8000,15000
```

Applies to both the pool-level retry (`pool_get_connection` Phase 5) and the step-level retry wrapper. Max 10 values; each value capped at 60000ms.

## Upgrading

Fresh installs pick up the index automatically. Existing installs: run `scripts/doctor.sh` or apply manually:

```sql
CREATE UNIQUE INDEX IF NOT EXISTS idx_blobs_linked_type_id_blob_type
ON plex.blobs(linked_type, linked_id, blob_type);
```

