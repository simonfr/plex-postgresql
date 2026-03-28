# Step Flow Architecture

## Scope

This document describes the runtime flow for `sqlite3_step` interception and the helper module boundaries.

## High-Level Flow

1. `my_sqlite3_step` (retry wrapper)
2. `my_sqlite3_step_impl` (orchestration)
3. Route by statement kind:
   - cached write
   - cached read
   - prepared read (eager or streaming)
   - prepared write
   - SQLite fallback

Connection-level failures are reported as `SQLITE_ERROR` with `step_pg_conn_error=1`, enabling retry in the wrapper.

## Module Boundaries

- `rust/plex-pg-core/src/db_interpose_step.rs`
  - orchestration only
  - route decisions and branch sequencing

- `rust/plex-pg-core/src/db_interpose_step_read_utils/`
  - `first_execute.rs` — read first execution path
  - `first_execute/connection.rs` — connection acquisition and locking
  - `first_execute/prepared_send.rs` — PQsendQueryPrepared
  - `first_execute/eager_fetch.rs` — full result fetch
  - `first_execute/streaming_fetch.rs` — single-row streaming (PQsetSingleRowMode)
  - `next_result.rs` — streaming/eager row advancement
  - `reexecution.rs` — metadata result re-execution
  - `support.rs` — cache clearing, helper functions
  - `play_queue_trace.rs` — play queue diagnostic tracing

- `rust/plex-pg-core/src/db_interpose_step_write_utils/`
  - `connection.rs` — write connection prepare/recovery
  - `write_exec.rs` — regular write execute/finalize
  - `cached_write.rs` — cached write execute/finalize
  - `special_insert.rs` — metadata_items RETURNING id handling
  - `logging.rs` — write policy guards + debug hooks
  - `support.rs` — shared helpers

- `rust/plex-pg-core/src/db_interpose_step_cached_read_utils.rs`
  - cached-read prepare/execute/finalize helpers

- `rust/plex-pg-core/src/db_interpose_conn_utils.rs`
  - shared connection cancel+drain utility (fast path: skips PQcancel when idle)

- `rust/plex-pg-core/src/db_interpose_stmt_lifecycle/`
  - `statement_ops.rs` — reset/finalize/clear_bindings interception
  - `ring_tracker.rs` — finalized/prepared statement diagnostic rings (Mutex-guarded)

## Result/Error Contract

Step helpers use `step_result_t`:

- `STEP_RESULT_ROW`
- `STEP_RESULT_DONE`
- `STEP_RESULT_ERROR`
- `STEP_RESULT_FALLBACK`

For helper APIs with `conn_error_out`:

- `conn_error_out=1` means a connection-level failure occurred and retry is appropriate.
- `conn_error_out=0` means non-connection logic/SQL path.

## Lock Ownership Convention

- `pg_stmt.mutex` is a Rust `std::sync::Mutex<()>` (non-recursive). Locked via `PgStmt::lock_mutex()`.
- `pg_conn.mutex` is a `pthread_mutex_t` (`PTHREAD_MUTEX_RECURSIVE`). Locked via `PthreadMutexGuard`.
- Lock order: **stmt before conn**. Never acquire conn mutex while holding stmt mutex in the same thread (the metadata path releases stmt mutex first).
- No logging (`log_debug_lazy!`, `log_info_lazy!`) while any mutex is held.
- Helpers may release `pg_stmt.mutex` via `drop(guard)` on terminal returns.
- `exec_conn.mutex` is acquired/released inside connection/execute helpers.

## PgStmt Memory Model

- PgStmt is allocated via `Box::new(PgStmt::new())` and managed through `Box::into_raw()` / `Box::from_raw()`.
- Parameter arrays (`param_values`, `param_buffers`, etc.) are `Vec<T>` — sized via `ensure_param_capacity()` at prepare time.
- Column cache arrays are `Vec<T>` — sized via `ensure_column_capacity()` at first step.
- All Vecs start empty; indexing before `ensure_*_capacity()` will panic.
- Drop of PgStmt automatically frees all Vec heap memory.
