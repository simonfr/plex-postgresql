/*
 * Plex PostgreSQL Interposing Shim - Step/Reset/Finalize Operations
 *
 * Handles sqlite3_step, sqlite3_reset, sqlite3_finalize, sqlite3_clear_bindings.
 * This is the main query execution module.
 */

#include "db_interpose.h"
#include "db_interpose_common.h"  // For platform_print_backtrace
#include "db_interpose_rust.h"
#include "db_interpose_step_cached_read_utils.h"
#include "db_interpose_step_read_utils.h"
#include "db_interpose_step_write_utils.h"
#include "pg_query_cache.h"
#include "shim_alloc.h"

// Forward declaration of the inner implementation
static int my_sqlite3_step_impl(sqlite3_stmt *pStmt);

// Thread-local flag: set to 1 by step_impl whenever SQLITE_ERROR is caused by
// a PG connection failure (not a SQL/logic error). Checked by the retry wrapper.
static __thread int step_pg_conn_error = 0;

// ============================================================================
// Step Function - Retry Wrapper (v0.9.34, fixes #8)
// ============================================================================
// When PG restarts, threads that already hold a pool connection see
// CONNECTION_BAD / PQsend failures. This wrapper catches SQLITE_ERROR,
// resets statement state, waits with exponential backoff, and retries.
// Every "return SQLITE_ERROR" inside step_impl releases all mutexes,
// so re-entry is safe and deadlock-free.

int my_sqlite3_step(sqlite3_stmt *pStmt) {
    static __thread int step_retry_count = 0;

    pg_stmt_t *dbg_stmt = pg_find_stmt(pStmt);
    const char *dbg_sql = NULL;
    sqlite3 *dbg_db = NULL;
    if (dbg_stmt) {
        dbg_sql = dbg_stmt->pg_sql ? dbg_stmt->pg_sql : dbg_stmt->sql;
    }
    if (!dbg_sql && orig_sqlite3_sql) {
        dbg_sql = orig_sqlite3_sql(pStmt);
    }
    if (orig_sqlite3_db_handle) {
        dbg_db = orig_sqlite3_db_handle(pStmt);
    }
    pg_exception_note_phase("step", dbg_sql, pStmt, dbg_db);

    int rc = my_sqlite3_step_impl(pStmt);

    // Backoff schedule from PLEX_PG_RETRY_DELAYS (default: 500,1000,2000,3000,4000 ms)
    int step_retry_delays_ms[PG_RETRY_MAX_DELAYS];
    int step_max_retries = 0;
    pg_get_retry_delays(step_retry_delays_ms, &step_max_retries);

    if (rc == SQLITE_ERROR && step_retry_count < step_max_retries) {
        pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
        if (pg_stmt && pg_stmt->is_pg && step_pg_conn_error) {
            // step_impl set step_pg_conn_error=1: error was connection-related
            step_pg_conn_error = 0;
            {
                int delay = step_retry_delays_ms[step_retry_count];
                step_retry_count++;
                LOG_ERROR("step: PG conn error, retry %d/%d in %dms (thread %p)",
                         step_retry_count, step_max_retries, delay, (void*)pthread_self());

                pthread_mutex_lock(&pg_stmt->mutex);
                pg_stmt_clear_result(pg_stmt);
                pthread_mutex_unlock(&pg_stmt->mutex);

                usleep(delay * 1000);
                step_pg_conn_error = 0;
                rc = my_sqlite3_step(pStmt);  // recursive retry

                if (step_retry_count > 0 && rc != SQLITE_ERROR) {
                    LOG_ERROR("step: retry succeeded after %d attempt(s)",
                             step_retry_count);
                }
                step_retry_count = 0;
                return rc;
            }
        }
    }

    if (step_retry_count > 0) {
        if (rc == SQLITE_ERROR) {
            LOG_ERROR("step: retries exhausted, returning SQLITE_ERROR");
        }
        // Success in recursive frame: reset counter silently
        step_retry_count = 0;
    }
    return rc;
}

// ============================================================================
// Step Function - Inner Implementation
// ============================================================================

static int my_sqlite3_step_impl(sqlite3_stmt *pStmt) {
    // Periodic shim memory usage summary (every 60s, near-zero overhead)
    shim_alloc_maybe_log();

    // CRITICAL FIX v0.9.3: If we're inside resolve_column_tables, skip shim entirely
    // This prevents: resolve_column_tables → PQexec → (Plex hook) → my_sqlite3_step → recursion
    // Flag declared in db_interpose.h, defined in db_interpose_common.c
    if (in_resolve_tables) {
        // Pass through to original SQLite - no PostgreSQL involved
        return orig_sqlite3_step ? orig_sqlite3_step(pStmt) : SQLITE_ERROR;
    }
    
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);

    // CRITICAL FIX v0.9.0: Set in_step flag to prevent concurrent bind
    if (pg_stmt) {
        atomic_store(&pg_stmt->in_step, 1);
    }

    // Skip statements
    if (pg_stmt && pg_stmt->is_pg == 3) {
        LOG_DEBUG("[RACE_DEBUG] STEP_END thread=%p stmt=%p rc=%d reason=skip", 
                  (void*)pthread_self(), (void*)pStmt, SQLITE_DONE);
        return SQLITE_DONE;
    }

    // Handle cached statements (prepared before our shim)
    if (!pg_stmt) {
        sqlite3 *db = sqlite3_db_handle(pStmt);
        pg_connection_t *pg_conn = pg_find_connection(db);
        if (!pg_conn) pg_conn = pg_find_any_library_connection();

        // v0.9.4.6: Only handle cached statements for library.db
        // Non-library databases should use SQLite directly
        if (pg_conn && pg_conn->is_pg_active && pg_conn->conn &&
            is_library_db_path(pg_conn->db_path)) {
            char *expanded_sql = sqlite3_expanded_sql(pStmt);
            const char *sql = expanded_sql ? expanded_sql : sqlite3_sql(pStmt);

            // Handle cached WRITE
            // Check both expanded SQL and original SQL for skip patterns
            const char *orig_sql = sqlite3_sql(pStmt);
            if (sql && is_write_operation(sql) && !should_skip_sql(sql) && !should_skip_sql(orig_sql)) {
                // Debug: log cached INSERT for metadata_items
                if (sql && strcasestr(sql, "INSERT") && strcasestr(sql, "metadata_items")) {
                    LOG_DEBUG("CACHED INSERT metadata_items:");
                    LOG_DEBUG("  expanded_sql=%s", expanded_sql ? "YES" : "NO");
                    LOG_DEBUG("  sql (first 300): %.300s", sql ? sql : "(null)");
                }
                // GUARD: Block cached junk INSERTs into metadata_items.
                // For cached statements, expanded_sql includes literal values.
                if (sql && rust_is_junk_metadata_insert(sql)) {
                    LOG_ERROR("GUARD: Blocked cached junk INSERT into metadata_items "
                              "(library_section_id=NULL, metadata_type=NULL)");
                    if (expanded_sql) sqlite3_free(expanded_sql);
                    return SQLITE_DONE;
                }

                // CRITICAL FIX: Check if this cached write was already executed
                pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
                if (cached && cached->write_executed) {
                    // Already executed, prevent duplicate execution
                    if (expanded_sql) sqlite3_free(expanded_sql);
                    return SQLITE_DONE;
                }

                pg_connection_t *cached_exec_conn = NULL;
                // For cached writes this picks per-thread connection and handles txn no-op.
                if (step_cached_write_should_noop(pg_conn, sql, &cached_exec_conn)) {
                    if (expanded_sql) sqlite3_free(expanded_sql);
                    return SQLITE_DONE;
                }

                sql_translation_t trans = sql_translate(sql);
                if (trans.success && trans.sql) {
                    const char *exec_sql = trans.sql;
                    char *insert_sql = step_cached_write_build_exec_sql(sql, trans.sql, &exec_sql);
                    int cached_write_conn_error = 0;
                    step_result_t cached_write_rc = step_cached_write_execute_and_finalize(
                        &cached, pStmt, pg_conn, cached_exec_conn, sql, exec_sql, &cached_write_conn_error);
                    if (insert_sql) free(insert_sql);
                    if (cached_write_rc == STEP_RESULT_ERROR) {
                        sql_translation_free(&trans);
                        if (expanded_sql) sqlite3_free(expanded_sql);
                        if (cached_write_conn_error) {
                            do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                        }
                        return SQLITE_ERROR;
                    }
                }
                sql_translation_free(&trans);
                if (expanded_sql) sqlite3_free(expanded_sql);
                return SQLITE_DONE;
            }

            // Handle cached READ
            if (sql && is_read_operation(sql) && !should_skip_sql(sql)) {
                // For cached statements, use per-thread pool connection when available.
                pg_connection_t *cached_read_conn = step_pick_thread_connection(pg_conn);
                step_result_t cached_branch_rc = STEP_RESULT_FALLBACK;
                pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
                int sqlite_result = orig_sqlite3_step ? orig_sqlite3_step(pStmt) : SQLITE_ERROR;

                if (sqlite_result == SQLITE_ROW || sqlite_result == SQLITE_DONE) {
                    step_result_t cached_rc = STEP_RESULT_DONE;
                    if (step_cached_read_finalize_advance(cached, expanded_sql, &cached_rc)) {
                        return cached_rc;
                    }

                    // No result yet - execute PostgreSQL query
                    sql_translation_t trans = sql_translate(sql);
                    if (trans.success && trans.sql) {
                        pg_stmt_t *new_stmt =
                            step_cached_read_prepare_stmt(cached, cached_read_conn, sql, pStmt, trans.sql);
                        if (new_stmt) {
                            int conn_error = 0;
                            cached_branch_rc = step_cached_read_execute(
                                new_stmt, cached_read_conn, sql, trans.sql, &conn_error);
                            if (conn_error && cached_branch_rc == STEP_RESULT_ERROR) {
                                step_pg_conn_error = 1;
                            }
                        }
                    }
                    sql_translation_free(&trans);
                }

                if (cached_branch_rc == STEP_RESULT_ROW ||
                    cached_branch_rc == STEP_RESULT_DONE ||
                    cached_branch_rc == STEP_RESULT_ERROR) {
                    if (expanded_sql) sqlite3_free(expanded_sql);
                    if (cached_branch_rc == STEP_RESULT_ERROR) return SQLITE_ERROR;
                    return cached_branch_rc;
                }

                if (expanded_sql) sqlite3_free(expanded_sql);
                return sqlite_result;
            }

            if (expanded_sql) sqlite3_free(expanded_sql);
        }
    }

    // Execute prepared statement on PostgreSQL
    // IMPORTANT: Use thread-local connection, not the one stored at prepare time
    // This ensures INSERT and SELECT on the same thread use the same connection
    // v0.9.4.5: Get connection from statement's db handle, not stored conn
    // This fixes the bug where stmt_conn was from wrong database (blobs.db vs library.db)
    pg_connection_t *exec_conn = NULL;

    if (pg_stmt && pg_stmt->shadow_stmt) {
        // Get the actual database handle from the statement
        sqlite3 *db = sqlite3_db_handle(pg_stmt->shadow_stmt);
        pg_connection_t *handle_conn = pg_find_connection(db);

        if (handle_conn && handle_conn->is_pg_active && is_library_db_path(handle_conn->db_path)) {
            // v0.9.5: Check if this is library.db (uses pool) or blobs.db (uses direct connection)
            // library.db has conn=NULL (pool-only), blobs.db has conn=PGconn* (direct)
            if (handle_conn->conn) {
                // Direct connection (blobs.db) - use it directly
                exec_conn = handle_conn;
            } else {
                // Pool connection (library.db) - get thread-local pool connection
                pg_connection_t *thread_conn = pg_get_thread_connection(handle_conn->db_path);
                if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
                    exec_conn = thread_conn;
                    // CRITICAL FIX: Touch connection IMMEDIATELY after obtaining it
                    pg_pool_touch_connection(exec_conn);
                }
            }
        }
    }

    if (pg_stmt && pg_stmt->pg_sql && exec_conn && exec_conn->conn) {
        // Lock statement mutex to protect statement state
        // NOTE: exec_conn->mutex is NOT needed because each thread has its own
        // connection from the pool (per-thread connection model)
        pthread_mutex_lock(&pg_stmt->mutex);

        const char *paramValues[MAX_PARAMS] = {NULL};  // Initialize to prevent garbage access
        for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
            paramValues[i] = pg_stmt->param_values[i];
        }

        if (pg_stmt->is_pg == 2) {  // READ
            // CRITICAL FIX: Prevent re-execution after SQLITE_DONE was returned
            // Without this, Plex calling step() after DONE would re-execute the query
            if (pg_stmt->read_done) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_DONE;
            }

            // CRITICAL FIX: Handle cached results FIRST, before checking pg_stmt->result
            // When using cache, result is NULL but cached_result is set.
            // The cache hit (below) sets current_row=0 and returns immediately.
            // On subsequent step() calls, we advance current_row here.
            if (pg_stmt->cached_result) {
                return step_read_advance_cached_result(pg_stmt);
            }

            // Only log when result is NULL (new query) to reduce log spam
            if (!pg_stmt->result) {
                LOG_DEBUG("STEP READ: thread=%p stmt=%p exec_conn=%p",
                         (void*)pthread_self(), (void*)pg_stmt, (void*)exec_conn);
            }

            // Shared statement used by different thread — clear stale result and re-execute
            // This is expected when Plex reuses prepared statements across threads
            if (pg_stmt->result && pg_stmt->result_conn != exec_conn) {
                LOG_DEBUG("STEP: Re-executing on current thread's connection (stmt shared across threads, result_conn=%p exec_conn=%p)",
                         (void*)pg_stmt->result_conn, (void*)exec_conn);
                PQclear(pg_stmt->result);
                pg_stmt->result = NULL;
                pg_stmt->result_conn = NULL;
                pg_stmt->current_row = 0;
            }

            // v0.8.9.1: Check if we need to re-execute due to metadata-only result
            // When bind() was called after metadata execution, it set metadata_only_result=2
            // to indicate we need to re-execute with the now-bound parameters
            if (pg_stmt->result && pg_stmt->metadata_only_result == 2) {
                LOG_DEBUG("STEP: Clearing metadata-only result for re-execution with bound params");
                PQclear(pg_stmt->result);
                pg_stmt->result = NULL;
                pg_stmt->metadata_only_result = 0;
                pg_stmt->current_row = -1;
            }

            // ================================================================
            // v0.9.28: SINGLE-ROW STREAMING MODE
            // Instead of fetching entire PGresult eagerly, we use
            // PQsetSingleRowMode to fetch one row at a time.
            // This matches SQLite's step() memory model: one row in memory.
            // ================================================================

             // === STREAMING: Subsequent step() — fetch next row ===
            if (pg_stmt->streaming_mode) {
                return step_read_streaming_next(pStmt, pg_stmt);
            }

            // === NON-STREAMING: Subsequent step() on eager result — advance row ===
            if (pg_stmt->result) {
                return step_read_eager_next(pg_stmt);
            }

            // === FIRST step() — send query and decide streaming vs eager ===
            if (!pg_stmt->result) {
                int conn_error = 0;
                step_result_t first_rc = step_read_first_execute(
                    pg_stmt, &exec_conn, paramValues, &conn_error);
                if (first_rc == STEP_RESULT_ERROR && conn_error) step_pg_conn_error = 1;
                return first_rc;
            }
        } else if (pg_stmt->is_pg == 1) {  // WRITE
            // CRITICAL FIX: Prevent duplicate execution of the same write
            // If Plex calls step() multiple times without reset(), only execute once
            if (pg_stmt->write_executed) {
                // Already executed this write, just return DONE
                // This prevents the statistics_media INSERT storm bug
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_DONE;
            }

            // Treat COMMIT/ROLLBACK/END as no-op when there is no active tx.
            // This matches SQLite behavior and avoids noisy PG warnings.
            int txn_state = PQTRANS_IDLE;
            if (step_pg_write_should_noop(exec_conn, pg_stmt ? pg_stmt->pg_sql : NULL, &txn_state)) {
                LOG_DEBUG("TXN_NOOP: skipping tx terminator in state=%d sql=%.120s",
                          txn_state, pg_stmt->pg_sql);
                pg_stmt->write_executed = 1;
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_DONE;
            }

            step_write_log_debug_context(pg_stmt, exec_conn, paramValues);

            if (step_write_should_skip_special_insert(pg_stmt, exec_conn, paramValues)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_DONE;
            }

            int prep_conn_error = 0;
            step_result_t prep_rc = step_write_prepare_connection(pg_stmt, &exec_conn, &prep_conn_error);
            if (prep_rc == STEP_RESULT_ERROR) {
                if (prep_conn_error) {
                    do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                }
                return SQLITE_ERROR;
            }

            int write_conn_error = 0;
            step_result_t write_rc = step_write_execute_and_finalize(
                pg_stmt, exec_conn, paramValues, &write_conn_error);
            if (write_rc == STEP_RESULT_ERROR) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                if (write_conn_error) {
                    do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                }
                return SQLITE_ERROR;
            }
        }

        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    if (pg_stmt && pg_stmt->is_pg) {
        // v0.8.9.5: WRITE statements always return SQLITE_DONE
        // SOCI expects this and uses last_insert_rowid() to get the ID via lastval()
        // The RETURNING result is kept for debugging but not exposed as SQLITE_ROW
        if (pg_stmt->is_pg == 1) return SQLITE_DONE;
    
    step_log_step_exit_trace(pg_stmt);
    }

    // Fallback to SQLite for non-PostgreSQL statements
    int final_rc = orig_sqlite3_step ? orig_sqlite3_step(pStmt) : SQLITE_ERROR;
    return final_rc;
}

// ============================================================================
// Reset/Finalize/Clear Bindings
// ============================================================================

static int reset_pg_stmt_locked(sqlite3_stmt *pStmt, pg_stmt_t *stmt) {
    pthread_mutex_lock(&stmt->mutex);

    // CRITICAL FIX v0.9.0: Clear in_step flag to allow new bind operations
    atomic_store(&stmt->in_step, 0);

    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->param_values[i] && !is_preallocated_buffer(stmt, i)) {
            free(stmt->param_values[i]);
            stmt->param_values[i] = NULL;
        }
    }
    pg_stmt_clear_result(stmt);  // This also resets write_executed

    int rc = SQLITE_OK;
    if (stmt->is_pg != 2) {
        // CRITICAL FIX: Call orig_sqlite3_reset WHILE HOLDING THE MUTEX
        // to prevent "bind on busy prepared statement" race condition
        rc = orig_sqlite3_reset ? orig_sqlite3_reset(pStmt) : SQLITE_ERROR;
    }

    pthread_mutex_unlock(&stmt->mutex);
    return rc;
}

int my_sqlite3_reset(sqlite3_stmt *pStmt) {
    // Clear prepared statements
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt) {
        return reset_pg_stmt_locked(pStmt, pg_stmt);
    }

    // Also clear cached statements - these use a separate registry
    pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
    if (cached) {
        return reset_pg_stmt_locked(pStmt, cached);
    }

    return orig_sqlite3_reset ? orig_sqlite3_reset(pStmt) : SQLITE_ERROR;
}

int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    int is_pg_only = 0;

    if (pg_stmt) {
        // Check if this is a PostgreSQL-only statement before cleaning up
        is_pg_only = (pg_stmt->is_pg == 2);

        // Statement is in global registry
        // Check if it's also in TLS cache - if so, need to decrement the TLS reference too
        pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
        if (cached == pg_stmt) {
            // Same statement in both caches - TLS added an extra reference
            // Use normal clear to properly decrement the ref_count
            LOG_DEBUG("finalize: stmt in both global and TLS, clearing TLS ref");
            pg_clear_cached_stmt(pStmt);
        } else if (cached) {
            // Different pg_stmt in TLS vs global — can happen when statement is
            // re-prepared on a different thread while TLS still has old version
            LOG_INFO("finalize: different pg_stmt in global vs TLS for same sqlite_stmt (cross-thread re-prepare)");
            pg_clear_cached_stmt(pStmt);
        }

        pg_unregister_stmt(pStmt);
        pg_stmt_unref(pg_stmt);
    } else {
        // Statement might only be in TLS cache - check it too
        pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
        if (cached) {
            is_pg_only = (cached->is_pg == 2);
            // TLS-only statement - but pg_register_cached_stmt incremented ref_count
            // so it's now at 2 instead of 1. Need to unref twice.
            LOG_DEBUG("finalize: stmt only in TLS (ref_count=%d), clearing",
                     atomic_load(&cached->ref_count));
            pg_clear_cached_stmt(pStmt);  // This unrefs once
            pg_stmt_unref(cached);         // Unref again to actually free
        }
    }

    // If this was a PostgreSQL-only statement, don't call real SQLite
    if (is_pg_only) {
        return SQLITE_OK;
    }

    return orig_sqlite3_finalize ? orig_sqlite3_finalize(pStmt) : SQLITE_ERROR;
}

int my_sqlite3_clear_bindings(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt) {
        // CRITICAL FIX: Lock mutex for entire operation to prevent race conditions
        pthread_mutex_lock(&pg_stmt->mutex);
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i] && !is_preallocated_buffer(pg_stmt, i)) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
        }
        int rc = orig_sqlite3_clear_bindings ? orig_sqlite3_clear_bindings(pStmt) : SQLITE_ERROR;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return rc;
    }
    return orig_sqlite3_clear_bindings ? orig_sqlite3_clear_bindings(pStmt) : SQLITE_ERROR;
}
