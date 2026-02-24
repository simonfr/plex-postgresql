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

                    // Log cached INSERT on play_queue_generators
                    if (strstr(sql, "play_queue_generators")) {
                        LOG_DEBUG("CACHED INSERT play_queue_generators on thread %p conn %p",
                                (void*)pthread_self(), (void*)cached_exec_conn);
                    }

                    // CRITICAL: Touch connection to prevent pool from releasing it during query
                    pg_pool_touch_connection(cached_exec_conn);

                    // CRITICAL: Lock connection mutex to prevent concurrent libpq access
                    pthread_mutex_lock(&cached_exec_conn->mutex);

                    // TOCTOU FIX v0.9.4.2: Re-check conn after acquiring lock
                    if (!cached_exec_conn->conn) {
                        LOG_ERROR("CACHED EXEC: conn became NULL after lock (TOCTOU race)");
                        pthread_mutex_unlock(&cached_exec_conn->mutex);
                        if (insert_sql) free(insert_sql);
                        sql_translation_free(&trans);
                        if (expanded_sql) sqlite3_free(expanded_sql);
                        do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                    }

                    // v0.9.33: Guard cancel+drain — skip if connection is streaming
                    // (defense-in-depth: pool should never return a streaming connection)
                    if (!atomic_load(&cached_exec_conn->streaming_active)) {
                        // v0.9.29: Cancel + drain any pending results before executing
                        PQsetnonblocking(cached_exec_conn->conn, 0);
                        while (PQisBusy(cached_exec_conn->conn)) {
                            PQconsumeInput(cached_exec_conn->conn);
                        }
                        // Cancel any in-progress query first (streaming leftovers)
                        {
                            PGcancel *ce_cancel = PQgetCancel(cached_exec_conn->conn);
                            if (ce_cancel) {
                                char ce_errbuf[256];
                                PQcancel(ce_cancel, ce_errbuf, sizeof(ce_errbuf));
                                PQfreeCancel(ce_cancel);
                            }
                        }
                        PGresult *pending;
                        int drain_ce = 0;
                        while ((pending = PQgetResult(cached_exec_conn->conn)) != NULL) {
                            drain_ce++;
                            if (drain_ce <= 3)
                                LOG_INFO("CACHED EXEC: Drained orphaned result from connection %p (status=%d)",
                                         (void*)cached_exec_conn, PQresultStatus(pending));
                            PQclear(pending);
                            if (drain_ce > 1000) {
                                LOG_INFO("CACHED EXEC: Drain loop exceeded 1000 on %p — aborting drain", (void*)cached_exec_conn);
                                break;
                            }
                        }
                    }

                    // Use prepared statement for better performance (skip parse/plan)
                    uint64_t sql_hash = pg_hash_sql(exec_sql);
                    char stmt_name[32];
                    snprintf(stmt_name, sizeof(stmt_name), "ce_%llx", (unsigned long long)sql_hash);
                    
                    const char *cached_stmt_name = NULL;
                    PGresult *res;
                    if (pg_stmt_cache_lookup(cached_exec_conn, sql_hash, &cached_stmt_name)) {
                        // Cached - execute prepared
                        res = PQexecPrepared(cached_exec_conn->conn, cached_stmt_name, 0, NULL, NULL, NULL, 0);
                    } else {
                        // Not cached - prepare and execute
                        PGresult *prep_res = PQprepare(cached_exec_conn->conn, stmt_name, exec_sql, 0, NULL);
                        if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                            pg_stmt_cache_add(cached_exec_conn, sql_hash, stmt_name, 0);
                            PQclear(prep_res);
                            res = PQexecPrepared(cached_exec_conn->conn, stmt_name, 0, NULL, NULL, NULL, 0);
                        } else if (pg_is_duplicate_prepared_stmt(prep_res)) {
                            pg_stmt_cache_add(cached_exec_conn, sql_hash, stmt_name, 0);
                            PQclear(prep_res);
                            res = PQexecPrepared(cached_exec_conn->conn, stmt_name, 0, NULL, NULL, NULL, 0);
                        } else {
                            LOG_DEBUG("CACHED EXEC prepare failed, using PQexec: %s",
                                      PQerrorMessage(cached_exec_conn->conn));
                            PQclear(prep_res);
                            res = PQexec(cached_exec_conn->conn, exec_sql);
                        }
                    }
                    pthread_mutex_unlock(&cached_exec_conn->mutex);
                    ExecStatusType status = PQresultStatus(res);

                    if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
                        pg_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

                        if (strncasecmp(sql, "INSERT", 6) == 0 && status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                            const char *id_str = PQgetvalue(res, 0, 0);
                            if (id_str && *id_str) {
                                sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(sql);
                                if (meta_id > 0) pg_set_global_metadata_id(meta_id);
                            }
                        }
                    } else {
                        const char *err = (pg_conn && pg_conn->conn) ? PQerrorMessage(pg_conn->conn) : "NULL connection";
                        log_sql_fallback(sql, exec_sql, err, "CACHED WRITE");
                        // v0.9.38: Stale prepared statement recovery (SQLSTATE 26000)
                        if (pg_is_stale_prepared_stmt(res)) {
                            pg_stmt_cache_clear_local(cached_exec_conn);
                            if (insert_sql) free(insert_sql);
                            PQclear(res);
                            /* Note: mutex already unlocked at line 310 — do NOT unlock again */
                            sql_translation_free(&trans);
                            if (expanded_sql) sqlite3_free(expanded_sql);
                            do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                        }
                        // CRITICAL: Check if connection is corrupted and needs reset
                        pg_pool_check_connection_health(cached_exec_conn);
                    }

                    if (insert_sql) free(insert_sql);
                    PQclear(res);

                    // CRITICAL FIX: Create cached stmt entry and mark as executed
                    if (!cached) {
                        cached = pg_stmt_create(cached_exec_conn, sql, pStmt);
                        if (cached) {
                            cached->is_pg = 1;  // WRITE
                            cached->is_cached = 1;
                            cached->write_executed = 1;  // Mark as executed
                            pg_register_cached_stmt(pStmt, cached);
                        }
                    } else {
                        cached->write_executed = 1;  // Mark as executed
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
                int cached_branch_rc = STEP_CACHED_READ_UNHANDLED;
                pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
                int sqlite_result = orig_sqlite3_step ? orig_sqlite3_step(pStmt) : SQLITE_ERROR;

                if (sqlite_result == SQLITE_ROW || sqlite_result == SQLITE_DONE) {
                    int cached_rc = SQLITE_DONE;
                    if (step_cached_read_maybe_advance(cached, expanded_sql, &cached_rc)) {
                        return cached_rc;
                    }

                    // No result yet - execute PostgreSQL query
                    sql_translation_t trans = sql_translate(sql);
                    if (trans.success && trans.sql) {
                        pg_stmt_t *new_stmt =
                            step_cached_read_get_or_create_stmt(cached, cached_read_conn, sql, pStmt, trans.sql);
                        if (new_stmt) {
                            int conn_error = 0;
                            cached_branch_rc = step_cached_read_execute_translated(
                                new_stmt, cached_read_conn, sql, trans.sql, &conn_error);
                            if (conn_error && cached_branch_rc == SQLITE_ERROR) {
                                step_pg_conn_error = 1;
                            }
                        }
                    }
                    sql_translation_free(&trans);
                }

                if (cached_branch_rc == SQLITE_ROW || cached_branch_rc == SQLITE_DONE || cached_branch_rc == SQLITE_ERROR) {
                    if (expanded_sql) sqlite3_free(expanded_sql);
                    if (cached_branch_rc == SQLITE_ERROR) return SQLITE_ERROR;
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
                int first_rc = step_read_first_execute(
                    pg_stmt, &exec_conn, paramValues, &conn_error);
                if (first_rc == SQLITE_ERROR && conn_error) step_pg_conn_error = 1;
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

            // Log INSERT on play_queue_generators for debugging
            if (pg_stmt->pg_sql && strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                LOG_DEBUG("INSERT play_queue_generators on thread %p conn %p",
                        (void*)pthread_self(), (void*)exec_conn);
            }

            // Debug: log INSERT params for troubleshooting
            if (pg_stmt->sql && strcasestr(pg_stmt->sql, "INSERT INTO metadata_items")) {
                LOG_DEBUG("STEP metadata_items INSERT: param_count=%d", pg_stmt->param_count);
                // CRITICAL FIX: Only access paramValues within bounds
                LOG_DEBUG("  PARAMS: [0]=%s [1]=%s [2]=%s [8]=%s [9]=%s",
                         (pg_stmt->param_count > 0 && paramValues[0]) ? paramValues[0] : "NULL",
                         (pg_stmt->param_count > 1 && paramValues[1]) ? paramValues[1] : "NULL",
                         (pg_stmt->param_count > 2 && paramValues[2]) ? paramValues[2] : "NULL",
                         (pg_stmt->param_count > 8 && paramValues[8]) ? paramValues[8] : "NULL",  // title
                         (pg_stmt->param_count > 9 && paramValues[9]) ? paramValues[9] : "NULL"); // title_sort
            }
            // Debug: log play_queue_generators INSERT params
            if (pg_stmt->sql && strcasestr(pg_stmt->sql, "play_queue_generators")) {
                LOG_DEBUG("STEP play_queue_generators INSERT: param_count=%d", pg_stmt->param_count);
                // CRITICAL FIX: Only access paramValues within bounds
                LOG_DEBUG("  PARAMS: [0]=%s [1]=%s [2]=%s [3]=%s",
                         (pg_stmt->param_count > 0 && paramValues[0]) ? paramValues[0] : "NULL",  // playlist_id
                         (pg_stmt->param_count > 1 && paramValues[1]) ? paramValues[1] : "NULL",  // metadata_item_id
                         (pg_stmt->param_count > 2 && paramValues[2]) ? paramValues[2] : "NULL",  // uri
                         (pg_stmt->param_count > 3 && paramValues[3]) ? paramValues[3] : "NULL"); // limit
                LOG_DEBUG("  SQL: %.300s", pg_stmt->pg_sql ? pg_stmt->pg_sql : "NULL");
            }

            // VALIDATION: Skip statistics_media INSERTs with empty count AND duration
            // FIX v0.9.2: Fetch sequence value before skipping to make last_insert_rowid() work
            if (pg_stmt->pg_sql && strcasestr(pg_stmt->pg_sql, "statistics_media")) {
                // Check if count (param 6) and duration (param 7) are both 0 or NULL
                const char *count_val = (pg_stmt->param_count > 6) ? paramValues[6] : NULL;
                const char *duration_val = (pg_stmt->param_count > 7) ? paramValues[7] : NULL;
                int count_empty = !count_val || strcmp(count_val, "0") == 0;
                int duration_empty = !duration_val || strcmp(duration_val, "0") == 0;
                
                if (count_empty && duration_empty) {
                    LOG_DEBUG("SKIP statistics_media INSERT: count=%s duration=%s (empty)",
                            count_val ? count_val : "NULL", duration_val ? duration_val : "NULL");
                    
                    // CRITICAL FIX: Advance the sequence so last_insert_rowid() works
                    // This prevents Plex from throwing std::exception on timeline requests
                    // CRITICAL FIX v0.9.4: Check NULL then lock BEFORE PQstatus (TOCTOU fix)
                    if (exec_conn && exec_conn->conn) {
                        pthread_mutex_lock(&exec_conn->mutex);
                        // TOCTOU FIX v0.9.4.2: Re-check conn after acquiring lock
                        if (!exec_conn->conn) {
                            LOG_ERROR("SKIP SEQ: conn became NULL after lock (TOCTOU race)");
                            pthread_mutex_unlock(&exec_conn->mutex);
                        } else if (PQstatus(exec_conn->conn) == CONNECTION_OK) {
                            PGresult *seq_res = PQexec(exec_conn->conn,
                                "SELECT nextval('plex.statistics_media_id_seq')");
                            if (PQresultStatus(seq_res) == PGRES_TUPLES_OK && PQntuples(seq_res) > 0) {
                                const char *seq_val = PQgetvalue(seq_res, 0, 0);
                                LOG_DEBUG("SKIP: Advanced sequence to %s", seq_val);
                            }
                            PQclear(seq_res);
                        }
                        pthread_mutex_unlock(&exec_conn->mutex);
                    }
                    
                    pg_stmt->write_executed = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }
            }

            // VALIDATION: Skip metadata_items INSERTs with NULL library_section_id AND metadata_type
            // These are junk rows created by misinterpreted bulk operations (40K+ found in production)
            if (pg_stmt->pg_sql && strcasestr(pg_stmt->pg_sql, "INSERT INTO") &&
                strcasestr(pg_stmt->pg_sql, "metadata_items") &&
                !strcasestr(pg_stmt->pg_sql, "metadata_item_settings") &&
                !strcasestr(pg_stmt->pg_sql, "metadata_item_views") &&
                !strcasestr(pg_stmt->pg_sql, "metadata_item_accounts") &&
                !strcasestr(pg_stmt->pg_sql, "metadata_item_clusters")) {
                int lib_idx = rust_find_insert_column_index(pg_stmt->pg_sql, "library_section_id");
                int type_idx = rust_find_insert_column_index(pg_stmt->pg_sql, "metadata_type");

                if (lib_idx >= 0 && type_idx >= 0 &&
                    lib_idx < pg_stmt->param_count && type_idx < pg_stmt->param_count) {
                    const char *lib_val = paramValues[lib_idx];
                    const char *type_val = paramValues[type_idx];
                    if (!lib_val && !type_val) {
                        LOG_ERROR("GUARD: Blocked junk INSERT into metadata_items "
                                  "(library_section_id=NULL, metadata_type=NULL) "
                                  "param_count=%d lib_idx=%d type_idx=%d",
                                  pg_stmt->param_count, lib_idx, type_idx);

                        // Advance sequence so last_insert_rowid() still works
                        if (exec_conn && exec_conn->conn) {
                            pthread_mutex_lock(&exec_conn->mutex);
                            if (exec_conn->conn && PQstatus(exec_conn->conn) == CONNECTION_OK) {
                                PGresult *seq_res = PQexec(exec_conn->conn,
                                    "SELECT nextval('plex.metadata_items_id_seq')");
                                if (PQresultStatus(seq_res) == PGRES_TUPLES_OK && PQntuples(seq_res) > 0) {
                                    LOG_DEBUG("GUARD: Advanced metadata_items sequence to %s",
                                              PQgetvalue(seq_res, 0, 0));
                                }
                                PQclear(seq_res);
                            }
                            pthread_mutex_unlock(&exec_conn->mutex);
                        }

                        pg_stmt->write_executed = 1;
                        pthread_mutex_unlock(&pg_stmt->mutex);
                        return SQLITE_DONE;
                    }
                }
            }

            // CRITICAL FIX v0.9.4: Check NULL but DON'T call PQstatus yet (TOCTOU fix)
            if (!exec_conn || !exec_conn->conn) {
                // v0.9.17: Retry once — pool may still be recovering after PG restart
                LOG_ERROR("STEP WRITE: NULL connection, retrying in 500ms (exec_conn=%p)",
                         (void*)exec_conn);
                pthread_mutex_unlock(&pg_stmt->mutex);
                usleep(500000);  // 500ms
                pthread_mutex_lock(&pg_stmt->mutex);

                sqlite3 *retry_db = sqlite3_db_handle(pg_stmt->shadow_stmt);
                pg_connection_t *retry_handle = pg_find_connection(retry_db);
                if (retry_handle && retry_handle->db_path[0]) {
                    exec_conn = pg_get_thread_connection(retry_handle->db_path);
                }
                if (!exec_conn || !exec_conn->conn) {
                    LOG_ERROR("STEP WRITE: NULL connection after retry — giving up");
                    pg_stmt->write_executed = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                }
                LOG_ERROR("STEP WRITE: reconnect retry succeeded (exec_conn=%p)", (void*)exec_conn);
            }

            // CRITICAL: Touch connection to prevent pool from releasing it during query
            pg_pool_touch_connection(exec_conn);

            // CRITICAL: Lock connection mutex BEFORE using conn (TOCTOU fix)
            pthread_mutex_lock(&exec_conn->mutex);

            // TOCTOU FIX v0.9.4.2: Re-check conn after acquiring lock
            if (!exec_conn->conn) {
                LOG_ERROR("STEP WRITE: conn became NULL after lock (TOCTOU race)");
                pthread_mutex_unlock(&exec_conn->mutex);
                pg_stmt->write_executed = 1;
                pthread_mutex_unlock(&pg_stmt->mutex);
                do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
            }

            // v0.9.33: TOCTOU guard — if connection became streaming after pool lookup
            if (atomic_load(&exec_conn->streaming_active)) {
                LOG_INFO("STEP WRITE: conn %p became streaming_active after lock, getting new connection",
                         (void*)exec_conn);
                pthread_mutex_unlock(&exec_conn->mutex);
                pg_connection_t *alt_conn = pg_get_thread_connection(
                    pg_stmt->conn ? pg_stmt->conn->db_path : NULL);
                if (alt_conn && alt_conn->conn && alt_conn != exec_conn &&
                    !atomic_load(&alt_conn->streaming_active)) {
                    exec_conn = alt_conn;
                    pg_pool_touch_connection(exec_conn);
                    pthread_mutex_lock(&exec_conn->mutex);
                    if (!exec_conn->conn || atomic_load(&exec_conn->streaming_active)) {
                        LOG_ERROR("STEP WRITE: alt conn also unavailable");
                        pthread_mutex_unlock(&exec_conn->mutex);
                        pg_stmt->write_executed = 1;
                        pthread_mutex_unlock(&pg_stmt->mutex);
                        do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                    }
                } else {
                    LOG_ERROR("STEP WRITE: no non-streaming connection available");
                    pg_stmt->write_executed = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                }
            }

            // Now safe to check connection status while holding lock
            ConnStatusType write_conn_status = PQstatus(exec_conn->conn);
            if (write_conn_status != CONNECTION_OK) {
                // Enhanced diagnostics for CONNECTION_BAD (v0.9.4.3)
                const char *pg_err = PQerrorMessage(exec_conn->conn);
                LOG_ERROR("=== CONNECTION_BAD DIAGNOSTIC (WRITE) ===");
                LOG_ERROR("  Status: %d, Thread: %p", (int)write_conn_status, (void*)pthread_self());
                LOG_ERROR("  Connection: %p, PGconn: %p", (void*)exec_conn, (void*)exec_conn->conn);
                LOG_ERROR("  PG Error: %s", pg_err ? pg_err : "(null)");
                LOG_ERROR("  SQL: %.100s", pg_stmt->sql ? pg_stmt->sql : "(null)");
                platform_print_backtrace("CONNECTION_BAD in STEP WRITE", 1);
                LOG_ERROR("=== END DIAGNOSTIC ===");
                LOG_ERROR("STEP WRITE: Attempting PQreset...");
                PQreset(exec_conn->conn);
                if (PQstatus(exec_conn->conn) != CONNECTION_OK) {
                    LOG_ERROR("STEP WRITE: PQreset failed, trying fresh PQconnectdb...");
                    pg_stmt_cache_clear(exec_conn);
                    PQfinish(exec_conn->conn);
                    exec_conn->conn = NULL;

                    pg_conn_config_t *wcfg = pg_config_get();
                    char wconninfo[1024];
                    snprintf(wconninfo, sizeof(wconninfo),
                             "host=%s port=%d dbname=%s user=%s password=%s "
                             "connect_timeout=5 keepalives=1 keepalives_idle=30 "
                             "keepalives_interval=10 keepalives_count=3",
                             wcfg->host, wcfg->port, wcfg->database, wcfg->user, wcfg->password);
                    PGconn *new_write_conn = PQconnectdb(wconninfo);
                    if (PQstatus(new_write_conn) == CONNECTION_OK) {
                        exec_conn->conn = new_write_conn;
                        exec_conn->is_pg_active = 1;
                        LOG_INFO("STEP WRITE: fresh connection succeeded (reconnected)");
                    } else {
                        const char *reset_err = PQerrorMessage(new_write_conn);
                        LOG_ERROR("STEP WRITE: fresh connection also failed: %s", reset_err ? reset_err : "(null)");
                        PQfinish(new_write_conn);
                        exec_conn->is_pg_active = 0;
                        pthread_mutex_unlock(&exec_conn->mutex);
                        pg_stmt->write_executed = 1;
                        pthread_mutex_unlock(&pg_stmt->mutex);
                        do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                    }
                } else {
                    LOG_ERROR("STEP WRITE: PQreset succeeded, connection recovered");
                }
            }

            // v0.9.33: Guard cancel+drain — skip if connection is streaming
            if (!atomic_load(&exec_conn->streaming_active)) {
                // v0.9.29: Cancel + drain any pending data before write
                PQsetnonblocking(exec_conn->conn, 0);
                while (PQisBusy(exec_conn->conn)) {
                    PQconsumeInput(exec_conn->conn);
                }
                // Cancel any in-progress query first (streaming leftovers)
                {
                    PGcancel *wr_cancel = PQgetCancel(exec_conn->conn);
                    if (wr_cancel) {
                        char wr_errbuf[256];
                        PQcancel(wr_cancel, wr_errbuf, sizeof(wr_errbuf));
                        PQfreeCancel(wr_cancel);
                    }
                }
                PGresult *pending;
                int drain_count = 0;
                while ((pending = PQgetResult(exec_conn->conn)) != NULL) {
                    drain_count++;
                    if (drain_count <= 3) {
                        LOG_INFO("STEP WRITE: Drained orphaned result from connection %p (status=%d: %s)",
                                  (void*)exec_conn, PQresultStatus(pending),
                                  PQresStatus(PQresultStatus(pending)));
                    }
                    PQclear(pending);
                    if (drain_count > 1000) {
                        LOG_INFO("STEP WRITE: Drain loop exceeded 1000 on %p — aborting drain", (void*)exec_conn);
                        break;
                    }
                }
                if (drain_count > 3) {
                    LOG_INFO("STEP WRITE: Drained %d orphaned results total from connection %p", drain_count, (void*)exec_conn);
                }
            }

            // Execute write
            PGresult *res = NULL;

            // Use prepared statements for better performance (skip parse/plan overhead)
            if (pg_stmt->use_prepared && pg_stmt->stmt_name[0]) {
                const char *cached_name = NULL;
                int is_cached = pg_stmt_cache_lookup(exec_conn, pg_stmt->sql_hash, &cached_name);

                if (!is_cached) {
                    // Prepare statement on this connection
                    PGresult *prep_res = PQprepare(exec_conn->conn, pg_stmt->stmt_name,
                                                    pg_stmt->pg_sql, pg_stmt->param_count, NULL);
                    if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                        pg_stmt_cache_add(exec_conn, pg_stmt->sql_hash, pg_stmt->stmt_name, pg_stmt->param_count);
                        cached_name = pg_stmt->stmt_name;
                        is_cached = 1;
                    } else if (pg_is_duplicate_prepared_stmt(prep_res)) {
                        pg_stmt_cache_add(exec_conn, pg_stmt->sql_hash, pg_stmt->stmt_name, pg_stmt->param_count);
                        cached_name = pg_stmt->stmt_name;
                        is_cached = 1;
                    } else {
                        LOG_DEBUG("PQprepare (write) failed for %s: %s", pg_stmt->stmt_name,
                                  PQerrorMessage(exec_conn->conn));
                    }
                    PQclear(prep_res);
                }

                if (is_cached && cached_name) {
                    // Execute prepared statement
                    res = PQexecPrepared(exec_conn->conn, cached_name,
                        pg_stmt->param_count, paramValues, NULL, NULL, 0);
                } else {
                    // Fallback to PQexecParams
                    res = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                        pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
                }
            } else {
                // No prepared statement support for this query
                res = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                    pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
            }

            pthread_mutex_unlock(&exec_conn->mutex);

            ExecStatusType status = PQresultStatus(res);
            if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
                exec_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

                // v0.8.9.5 FIX: For INSERT...RETURNING, log the ID but DON'T store result
                // SOCI uses lastval() via last_insert_rowid() to get the ID, not RETURNING columns
                // Storing result with current_row=-1 causes issues when column functions are called
                if (status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                    const char *id_str = PQgetvalue(res, 0, 0);
                    if (id_str && *id_str) {
                        sqlite3_int64 rowid = (sqlite3_int64)atoll(id_str);
                        if (rowid > 0) {
                            exec_conn->last_insert_rowid = rowid;
                            pg_set_global_last_insert_rowid(rowid);
                        }

                        if (pg_stmt->pg_sql && strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                            LOG_DEBUG("STEP play_queue_generators: RETURNING id = %s on thread %p conn %p",
                                    id_str, (void*)pthread_self(), (void*)exec_conn);
                        }
                        sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(pg_stmt->sql);
                        if (meta_id > 0) pg_set_global_metadata_id(meta_id);
                    }
                    // Don't store result - SOCI will use lastval() instead
                }
            } else {
                const char *err = (exec_conn && exec_conn->conn) ? PQerrorMessage(exec_conn->conn) : "NULL connection";
                LOG_ERROR("STEP PG write error: %s", err);
                LOG_ERROR("  Original SQL: %.300s", pg_stmt->sql ? pg_stmt->sql : "(null)");
                LOG_ERROR("  Translated SQL: %.300s", pg_stmt->pg_sql ? pg_stmt->pg_sql : "(null)");
                // v0.9.38: Stale prepared statement recovery (SQLSTATE 26000)
                if (pg_is_stale_prepared_stmt(res)) {
                    pg_stmt_cache_clear_local(exec_conn);
                    PQclear(res);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    do { step_pg_conn_error = 1; return SQLITE_ERROR; } while(0);
                }
                // CRITICAL: Check if connection is corrupted and needs reset
                pg_pool_check_connection_health(exec_conn);
            }

            // Mark as executed to prevent re-execution on subsequent step() calls
            pg_stmt->write_executed = 1;

            // v0.8.9.5: Keep RETURNING result for column access - don't clear it!
            // SQLite returns SQLITE_ROW for INSERT...RETURNING, then SQLITE_DONE on next step()
            if (res) PQclear(res);
        }

        pthread_mutex_unlock(&pg_stmt->mutex);
    }

    if (pg_stmt && pg_stmt->is_pg) {
        // v0.8.9.5: WRITE statements always return SQLITE_DONE
        // SOCI expects this and uses last_insert_rowid() to get the ID via lastval()
        // The RETURNING result is kept for debugging but not exposed as SQLITE_ROW
        if (pg_stmt->is_pg == 1) return SQLITE_DONE;
    
    // DEBUG TRACE: Log every step completion for PlayQueue/COUNT queries
    if (pg_stmt && pg_stmt->pg_sql) {
        int is_count = (strstr(pg_stmt->pg_sql, "COUNT(") != NULL || strstr(pg_stmt->pg_sql, "SUM(") != NULL || strstr(pg_stmt->pg_sql, "MAX(") != NULL);
        int is_playqueue = (strstr(pg_stmt->pg_sql, "play_queue") != NULL);
        
        if (is_count || is_playqueue) {
            LOG_DEBUG("DEBUG_TRACE: STEP_EXIT - rows=%d cols=%d sql=%.100s",
                      pg_stmt->num_rows, pg_stmt->num_cols, pg_stmt->pg_sql);
        }
    }
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
