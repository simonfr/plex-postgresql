/*
 * Plex PostgreSQL Interposing Shim - Step/Reset/Finalize Operations
 *
 * Handles sqlite3_step, sqlite3_reset, sqlite3_finalize, sqlite3_clear_bindings.
 * This is the main query execution module.
 */

#include "db_interpose.h"
#include "db_interpose_common.h"  // For platform_print_backtrace
#include "pg_query_cache.h"

// ============================================================================
// Step Function - Main Query Execution
// ============================================================================

int my_sqlite3_step(sqlite3_stmt *pStmt) {
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
                // GUARD: Block cached junk INSERTs into metadata_items
                // For cached stmts, expanded_sql has literal values inline
                if (sql && strcasestr(sql, "INSERT") && strcasestr(sql, "metadata_items") &&
                    !strcasestr(sql, "metadata_item_settings") &&
                    !strcasestr(sql, "metadata_item_views") &&
                    !strcasestr(sql, "metadata_item_accounts") &&
                    !strcasestr(sql, "metadata_item_clusters")) {
                    // Check if library_section_id and metadata_type are both NULL in the VALUES
                    // In expanded SQL, NULL columns appear as literal NULL in the VALUES list
                    // Parse: find VALUES(...) then check columns by position
                    const char *col_start = strchr(sql, '(');
                    if (col_start) {
                        int lib_idx = -1, type_idx = -1, idx = 0;
                        const char *p = col_start + 1;
                        while (*p && *p != ')') {
                            while (*p == ' ' || *p == '"' || *p == '`') p++;
                            if (strncmp(p, "library_section_id", 18) == 0) lib_idx = idx;
                            if (strncmp(p, "metadata_type", 13) == 0 &&
                                (p[13] == '"' || p[13] == '`' || p[13] == ',' || p[13] == ')' || p[13] == ' '))
                                type_idx = idx;
                            while (*p && *p != ',' && *p != ')') p++;
                            if (*p == ',') { p++; idx++; }
                        }
                        // Now find VALUES(...) and check the corresponding positions
                        const char *vals = strcasestr(sql, "VALUES");
                        if (vals && lib_idx >= 0 && type_idx >= 0) {
                            const char *vp = strchr(vals, '(');
                            if (vp) {
                                vp++;
                                int vi = 0;
                                int lib_null = 0, type_null = 0;
                                while (*vp && *vp != ')') {
                                    while (*vp == ' ') vp++;
                                    if (vi == lib_idx && strncasecmp(vp, "NULL", 4) == 0) lib_null = 1;
                                    if (vi == type_idx && strncasecmp(vp, "NULL", 4) == 0) type_null = 1;
                                    // Skip to next value (handle quoted strings)
                                    int in_quote = 0;
                                    while (*vp && (*vp != ',' || in_quote) && *vp != ')') {
                                        if (*vp == '\'') in_quote = !in_quote;
                                        vp++;
                                    }
                                    if (*vp == ',') { vp++; vi++; }
                                }
                                if (lib_null && type_null) {
                                    LOG_ERROR("GUARD: Blocked cached junk INSERT into metadata_items "
                                              "(library_section_id=NULL, metadata_type=NULL)");
                                    if (expanded_sql) sqlite3_free(expanded_sql);
                                    return SQLITE_DONE;
                                }
                            }
                        }
                    }
                }

                // CRITICAL FIX: Check if this cached write was already executed
                pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
                if (cached && cached->write_executed) {
                    // Already executed, prevent duplicate execution
                    if (expanded_sql) sqlite3_free(expanded_sql);
                    return SQLITE_DONE;
                }

                // For cached statements, also use thread-local connection
                pg_connection_t *cached_exec_conn = pg_conn;
                if (is_library_db_path(pg_conn->db_path)) {
                    pg_connection_t *thread_conn = pg_get_thread_connection(pg_conn->db_path);
                    if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
                        cached_exec_conn = thread_conn;
                    }
                }

                sql_translation_t trans = sql_translate(sql);
                if (trans.success && trans.sql) {
                    char *exec_sql = trans.sql;
                    char *insert_sql = convert_metadata_settings_insert_to_upsert(trans.sql);
                    if (insert_sql) {
                        exec_sql = insert_sql;
                    } else if (strncasecmp(sql, "INSERT", 6) == 0 &&
                               strcasestr(trans.sql, "schema_migrations") &&
                               !strcasestr(trans.sql, "ON CONFLICT")) {
                        // schema_migrations: add ON CONFLICT DO NOTHING (no RETURNING id)
                        size_t len = strlen(trans.sql);
                        insert_sql = malloc(len + 40);
                        if (insert_sql) {
                            snprintf(insert_sql, len + 40, "%s ON CONFLICT DO NOTHING", trans.sql);
                            exec_sql = insert_sql;
                        }
                    } else if (strncasecmp(sql, "INSERT", 6) == 0 && !strstr(trans.sql, "RETURNING") &&
                               !strcasestr(trans.sql, "schema_migrations")) {
                        size_t len = strlen(trans.sql);
                        insert_sql = malloc(len + 20);
                        if (insert_sql) {
                            snprintf(insert_sql, len + 20, "%s RETURNING id", trans.sql);
                            exec_sql = insert_sql;
                        }
                    }

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
                        return SQLITE_ERROR;
                    }

                    // Drain any pending results before executing
                    PQsetnonblocking(cached_exec_conn->conn, 0);
                    while (PQisBusy(cached_exec_conn->conn)) {
                        PQconsumeInput(cached_exec_conn->conn);
                    }
                    PGresult *pending;
                    while ((pending = PQgetResult(cached_exec_conn->conn)) != NULL) {
                        LOG_ERROR("CACHED EXEC: Drained orphaned result from connection %p", (void*)cached_exec_conn);
                        PQclear(pending);
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
                        } else {
                            // Prepare failed - fall back to PQexec
                            LOG_DEBUG("CACHED EXEC prepare failed, using PQexec: %s", PQerrorMessage(cached_exec_conn->conn));
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
                // For cached statements, also use thread-local connection
                pg_connection_t *cached_read_conn = pg_conn;
                if (is_library_db_path(pg_conn->db_path)) {
                    pg_connection_t *thread_conn = pg_get_thread_connection(pg_conn->db_path);
                    if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
                        cached_read_conn = thread_conn;
                    }
                }

                pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
                int sqlite_result = orig_sqlite3_step ? orig_sqlite3_step(pStmt) : SQLITE_ERROR;

                if (sqlite_result == SQLITE_ROW || sqlite_result == SQLITE_DONE) {
                    // For cached statements:
                    // - First call (no result): execute PostgreSQL query
                    // - Subsequent calls: advance through results
                    if (cached && cached->result) {
                        // Already have results, advance to next row
                        cached->current_row++;
                        if (cached->current_row >= cached->num_rows) {
                            // CRITICAL FIX: Free PGresult immediately when done
                            // Prevents memory accumulation when Plex doesn't call reset()
                            PQclear(cached->result);
                            cached->result = NULL;
                            if (expanded_sql) sqlite3_free(expanded_sql);
                            return SQLITE_DONE;
                        }
                        if (expanded_sql) sqlite3_free(expanded_sql);
                        return SQLITE_ROW;
                    }

                    // No result yet - execute PostgreSQL query
                    sql_translation_t trans = sql_translate(sql);
                    if (trans.success && trans.sql) {
                        // Verbose query logging disabled for performance

                        pg_stmt_t *new_stmt = cached;
                        if (!new_stmt) {
                            new_stmt = pg_stmt_create(cached_read_conn, sql, pStmt);
                            if (new_stmt) {
                                new_stmt->pg_sql = strdup(trans.sql);
                                new_stmt->is_pg = 2;
                                new_stmt->is_cached = 1;
                                pg_register_cached_stmt(pStmt, new_stmt);
                            }
                        }
                        if (new_stmt) {
                            // CRITICAL: Touch connection to prevent pool from releasing it during query
                            pg_pool_touch_connection(cached_read_conn);

                            // CRITICAL: Lock connection mutex to prevent concurrent libpq access
                            pthread_mutex_lock(&cached_read_conn->mutex);

                            // TOCTOU FIX v0.9.4.2: Re-check conn after acquiring lock
                            if (!cached_read_conn->conn) {
                                LOG_ERROR("CACHED READ: conn became NULL after lock (TOCTOU race)");
                                pthread_mutex_unlock(&cached_read_conn->mutex);
                                sql_translation_free(&trans);
                                if (expanded_sql) sqlite3_free(expanded_sql);
                                return SQLITE_ERROR;
                            }

                            // Drain any pending results before executing
                            PQsetnonblocking(cached_read_conn->conn, 0);
                            while (PQisBusy(cached_read_conn->conn)) {
                                PQconsumeInput(cached_read_conn->conn);
                            }
                            PGresult *pending_read;
                            while ((pending_read = PQgetResult(cached_read_conn->conn)) != NULL) {
                                LOG_ERROR("CACHED READ: Drained orphaned result from connection %p", (void*)cached_read_conn);
                                PQclear(pending_read);
                            }

                            // Use prepared statement for better performance (skip parse/plan)
                            uint64_t read_sql_hash = pg_hash_sql(trans.sql);
                            char read_stmt_name[32];
                            snprintf(read_stmt_name, sizeof(read_stmt_name), "cr_%llx", (unsigned long long)read_sql_hash);
                            
                            const char *cached_read_stmt_name = NULL;
                            if (pg_stmt_cache_lookup(cached_read_conn, read_sql_hash, &cached_read_stmt_name)) {
                                // Cached - execute prepared
                                LOG_DEBUG("CACHED READ (prepared): stmt=%s sql=%.60s", cached_read_stmt_name, trans.sql);
                                new_stmt->result = PQexecPrepared(cached_read_conn->conn, cached_read_stmt_name, 0, NULL, NULL, NULL, 0);
                            } else {
                                // Not cached - prepare and execute
                                PGresult *prep_res = PQprepare(cached_read_conn->conn, read_stmt_name, trans.sql, 0, NULL);
                                if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                                    pg_stmt_cache_add(cached_read_conn, read_sql_hash, read_stmt_name, 0);
                                    PQclear(prep_res);
                                    LOG_DEBUG("CACHED READ (new prepared): stmt=%s sql=%.60s", read_stmt_name, trans.sql);
                                    new_stmt->result = PQexecPrepared(cached_read_conn->conn, read_stmt_name, 0, NULL, NULL, NULL, 0);
                                } else {
                                    // Prepare failed - fall back to PQexec
                                    LOG_DEBUG("CACHED READ prepare failed, using PQexec: %s", PQerrorMessage(cached_read_conn->conn));
                                    PQclear(prep_res);
                                    new_stmt->result = PQexec(cached_read_conn->conn, trans.sql);
                                }
                            }
                            pthread_mutex_unlock(&cached_read_conn->mutex);
                            if (PQresultStatus(new_stmt->result) == PGRES_TUPLES_OK) {
                                new_stmt->num_rows = PQntuples(new_stmt->result);
                                new_stmt->num_cols = PQnfields(new_stmt->result);
                                new_stmt->current_row = 0;
                                new_stmt->result_conn = cached_read_conn;

                                // Resolve source table names for bare column lookup in decltype
                                if (resolve_column_tables(new_stmt, cached_read_conn) < 0) {
                                    LOG_ERROR("Failed to resolve column tables, cleaning up");
                                    // Don't fail the query - just log warning
                                    // Table resolution is for metadata only, not critical
                                }

                                // Verbose result logging disabled for performance
                                sql_translation_free(&trans);
                                if (expanded_sql) sqlite3_free(expanded_sql);
                                return (new_stmt->num_rows > 0) ? SQLITE_ROW : SQLITE_DONE;
                            } else {
                                const char *err = (cached_read_conn && cached_read_conn->conn) ? PQerrorMessage(cached_read_conn->conn) : "NULL connection";
                                log_sql_fallback(sql, trans.sql, err, "CACHED READ");
                                PQclear(new_stmt->result);
                                new_stmt->result = NULL;
                                // CRITICAL: Check if connection is corrupted and needs reset
                                pg_pool_check_connection_health(cached_read_conn);
                            }
                        }
                    }
                    sql_translation_free(&trans);
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
                // Advance to next row
                // Cache hit set current_row=0, so second step increments to 1, etc.
                pg_stmt->current_row++;
                if (pg_stmt->current_row >= pg_stmt->num_rows) {
                    // Done with cached result - release ref
                    pg_query_cache_release(pg_stmt->cached_result);
                    pg_stmt->cached_result = NULL;
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_ROW;
            }

            // Only log when result is NULL (new query) to reduce log spam
            if (!pg_stmt->result) {
                LOG_DEBUG("STEP READ: thread=%p stmt=%p exec_conn=%p",
                         (void*)pthread_self(), (void*)pg_stmt, (void*)exec_conn);
            }

            // CRITICAL FIX: Check if result belongs to a different connection
            // If statement is being used by a different thread/connection, we must
            // re-execute the query on THIS thread's connection to avoid protocol desync
            if (pg_stmt->result && pg_stmt->result_conn != exec_conn) {
                LOG_ERROR("STEP: Result from different connection! Clearing result (result_conn=%p exec_conn=%p)",
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
                // Clear previous single-row result
                if (pg_stmt->result) {
                    PQclear(pg_stmt->result);
                    pg_stmt->result = NULL;
                }

                // Clear cached text/blob for previous row
                for (int i = 0; i < MAX_PARAMS; i++) {
                    if (pg_stmt->cached_text[i]) { free(pg_stmt->cached_text[i]); pg_stmt->cached_text[i] = NULL; }
                    if (pg_stmt->cached_blob[i]) { free(pg_stmt->cached_blob[i]); pg_stmt->cached_blob[i] = NULL; pg_stmt->cached_blob_len[i] = 0; }
                    if (pg_stmt->decoded_blobs[i]) { free(pg_stmt->decoded_blobs[i]); pg_stmt->decoded_blobs[i] = NULL; pg_stmt->decoded_blob_lens[i] = 0; }
                }
                pg_stmt->cached_row = -1;
                pg_stmt->decoded_blob_row = -1;

                // Fetch next row (connection mutex NOT held — single-row mode
                // means the connection is exclusively ours until we drain all results)
                PGresult *row_res = PQgetResult(pg_stmt->streaming_conn->conn);
                if (!row_res) {
                    // NULL = no more results (shouldn't happen before TUPLES_OK sentinel)
                    LOG_DEBUG("STREAM: NULL result (connection done) stmt=%p", (void*)pStmt);
                    pg_stmt->streaming_mode = 0;
                    pg_stmt->streaming_conn = NULL;
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }

                ExecStatusType row_status = PQresultStatus(row_res);
                if (row_status == PGRES_SINGLE_TUPLE) {
                    // Got a row
                    pg_stmt->result = row_res;
                    pg_stmt->current_row = 0;  // Always row 0 in single-row result
                    pg_stmt->num_rows = 1;
                    pg_stmt->num_cols = PQnfields(row_res);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_ROW;
                } else if (row_status == PGRES_TUPLES_OK) {
                    // Empty sentinel = end of rows. Drain final NULL.
                    PQclear(row_res);
                    PGresult *final_null = PQgetResult(pg_stmt->streaming_conn->conn);
                    if (final_null) PQclear(final_null);  // Should be NULL, but be safe
                    pg_stmt->streaming_mode = 0;
                    pg_stmt->streaming_conn = NULL;
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                } else {
                    // Error during streaming
                    const char *err = PQerrorMessage(pg_stmt->streaming_conn->conn);
                    LOG_ERROR("STREAM ERROR: %s (status=%d) sql=%.100s",
                             err ? err : "(null)", (int)row_status, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
                    PQclear(row_res);
                    // Drain remaining results to free connection
                    PGresult *drain;
                    while ((drain = PQgetResult(pg_stmt->streaming_conn->conn)) != NULL) PQclear(drain);
                    pg_stmt->streaming_mode = 0;
                    pg_stmt->streaming_conn = NULL;
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;  // Treat as empty result rather than error
                }
            }

            // === NON-STREAMING: Subsequent step() on eager result — advance row ===
            if (pg_stmt->result) {
                pg_stmt->current_row++;
                if (pg_stmt->current_row >= pg_stmt->num_rows) {
                    PQclear(pg_stmt->result);
                    pg_stmt->result = NULL;
                    pg_stmt->result_conn = NULL;
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_ROW;
            }

            // === FIRST step() — send query and decide streaming vs eager ===
            if (!pg_stmt->result) {
                // Track which thread is executing this statement
                pthread_t current = pthread_self();
                pg_stmt->executing_thread = current;

                // CRITICAL FIX v0.9.4: Lock connection BEFORE status check to prevent TOCTOU race
                if (!exec_conn || !exec_conn->conn) {
                    LOG_ERROR("STEP SELECT: NULL connection, retrying in 500ms (exec_conn=%p)",
                             (void*)exec_conn);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    usleep(500000);
                    pthread_mutex_lock(&pg_stmt->mutex);

                    sqlite3 *retry_db = sqlite3_db_handle(pg_stmt->shadow_stmt);
                    pg_connection_t *retry_handle = pg_find_connection(retry_db);
                    if (retry_handle && retry_handle->db_path[0]) {
                        exec_conn = pg_get_thread_connection(retry_handle->db_path);
                    }
                    if (!exec_conn || !exec_conn->conn) {
                        LOG_ERROR("STEP SELECT: NULL connection after retry — giving up");
                        pthread_mutex_unlock(&pg_stmt->mutex);
                        return SQLITE_ERROR;
                    }
                    LOG_ERROR("STEP SELECT: reconnect retry succeeded (exec_conn=%p)", (void*)exec_conn);
                }

                pg_pool_touch_connection(exec_conn);
                pthread_mutex_lock(&exec_conn->mutex);

                if (!exec_conn->conn) {
                    LOG_ERROR("STEP SELECT: conn became NULL after lock (TOCTOU race)");
                    pthread_mutex_unlock(&exec_conn->mutex);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_ERROR;
                }

                // Connection recovery (same as before)
                ConnStatusType conn_status = PQstatus(exec_conn->conn);
                if (conn_status != CONNECTION_OK) {
                    const char *pg_err = PQerrorMessage(exec_conn->conn);
                    LOG_ERROR("=== CONNECTION_BAD DIAGNOSTIC (READ) ===");
                    LOG_ERROR("  Status: %d, Thread: %p", (int)conn_status, (void*)pthread_self());
                    LOG_ERROR("  Connection: %p, PGconn: %p", (void*)exec_conn, (void*)exec_conn->conn);
                    LOG_ERROR("  PG Error: %s", pg_err ? pg_err : "(null)");
                    LOG_ERROR("  SQL: %.100s", pg_stmt->sql ? pg_stmt->sql : "(null)");
                    platform_print_backtrace("CONNECTION_BAD in STEP READ", 1);
                    LOG_ERROR("=== END DIAGNOSTIC ===");
                    LOG_ERROR("STEP READ: Attempting PQreset...");
                    PQreset(exec_conn->conn);
                    if (PQstatus(exec_conn->conn) != CONNECTION_OK) {
                        LOG_ERROR("STEP READ: PQreset failed, trying fresh PQconnectdb...");
                        pg_stmt_cache_clear(exec_conn);
                        PQfinish(exec_conn->conn);
                        exec_conn->conn = NULL;

                        pg_conn_config_t *rcfg = pg_config_get();
                        char rconninfo[1024];
                        snprintf(rconninfo, sizeof(rconninfo),
                                 "host=%s port=%d dbname=%s user=%s password=%s "
                                 "connect_timeout=5 keepalives=1 keepalives_idle=30 "
                                 "keepalives_interval=10 keepalives_count=3",
                                 rcfg->host, rcfg->port, rcfg->database, rcfg->user, rcfg->password);
                        PGconn *new_read_conn = PQconnectdb(rconninfo);
                        if (PQstatus(new_read_conn) == CONNECTION_OK) {
                            exec_conn->conn = new_read_conn;
                            exec_conn->is_pg_active = 1;
                            LOG_ERROR("STEP READ: fresh connection succeeded (reconnected)");
                        } else {
                            const char *reset_err = PQerrorMessage(new_read_conn);
                            LOG_ERROR("STEP READ: fresh connection also failed: %s", reset_err ? reset_err : "(null)");
                            PQfinish(new_read_conn);
                            exec_conn->is_pg_active = 0;
                            pthread_mutex_unlock(&exec_conn->mutex);
                            pthread_mutex_unlock(&pg_stmt->mutex);
                            return SQLITE_ERROR;
                        }
                    } else {
                        LOG_ERROR("STEP READ: PQreset succeeded, connection recovered");
                    }
                    pg_conn_config_t *cfg = pg_config_get();
                    if (cfg) {
                        char schema_cmd[256];
                        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
                        PGresult *r = PQexec(exec_conn->conn, schema_cmd);
                        PQclear(r);
                        r = PQexec(exec_conn->conn, "SET statement_timeout = '5min'");
                        PQclear(r);
                    }
                }

                // Drain any pending data
                PQsetnonblocking(exec_conn->conn, 0);
                while (PQisBusy(exec_conn->conn)) {
                    PQconsumeInput(exec_conn->conn);
                }
                PGresult *pending;
                while ((pending = PQgetResult(exec_conn->conn)) != NULL) {
                    LOG_ERROR("STEP: Drained orphaned result from connection %p", (void*)exec_conn);
                    PQclear(pending);
                }

                // ============================================================
                // v0.9.28: Send query asynchronously for single-row streaming
                // ============================================================

                // v0.9.28: Raise statement_timeout for streaming queries.
                // The default 60s was too aggressive — some Plex migration/scan queries
                // legitimately take longer on large libraries.
                {
                    PGresult *to_res = PQexec(exec_conn->conn, "SET statement_timeout = '5min'");
                    PQclear(to_res);
                }

                int send_ok = 0;

                // Ensure prepared statement exists
                LOG_DEBUG("PREPARED CHECK: use_prepared=%d stmt_name[0]=%d pg_sql=%p",
                         pg_stmt->use_prepared, (int)pg_stmt->stmt_name[0], (void*)pg_stmt->pg_sql);
                if (pg_stmt->use_prepared && pg_stmt->stmt_name[0] && pg_stmt->pg_sql) {
                    const char *cached_name = NULL;
                    int is_cached = pg_stmt_cache_lookup(exec_conn, pg_stmt->sql_hash, &cached_name);

                    if (!is_cached) {
                        PGresult *prep_res = PQprepare(exec_conn->conn, pg_stmt->stmt_name,
                                                        pg_stmt->pg_sql, pg_stmt->param_count, NULL);
                        if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                            pg_stmt_cache_add(exec_conn, pg_stmt->sql_hash, pg_stmt->stmt_name, pg_stmt->param_count);
                            cached_name = pg_stmt->stmt_name;
                            is_cached = 1;
                        } else {
                            LOG_ERROR("PQprepare failed for %s: %s", pg_stmt->stmt_name, PQerrorMessage(exec_conn->conn));
                        }
                        PQclear(prep_res);
                    }

                    if (is_cached && cached_name) {
                        send_ok = PQsendQueryPrepared(exec_conn->conn, cached_name,
                            pg_stmt->param_count, paramValues, NULL, NULL, 0);
                    } else {
                        send_ok = PQsendQueryParams(exec_conn->conn, pg_stmt->pg_sql,
                            pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
                    }
                } else {
                    send_ok = PQsendQueryParams(exec_conn->conn, pg_stmt->pg_sql,
                        pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
                }

                if (!send_ok) {
                    const char *err = PQerrorMessage(exec_conn->conn);
                    LOG_ERROR("PQsend* failed: %s sql=%.200s", err ? err : "(null)", pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
                    pthread_mutex_unlock(&exec_conn->mutex);
                    pg_pool_check_connection_health(exec_conn);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_ERROR;
                }

                // Activate single-row mode
                if (!PQsetSingleRowMode(exec_conn->conn)) {
                    LOG_ERROR("PQsetSingleRowMode failed, falling back to eager fetch");
                    // Fallback: collect full result eagerly (old behavior)
                    pg_stmt->result = PQgetResult(exec_conn->conn);
                    // Drain final NULL
                    PGresult *trail;
                    while ((trail = PQgetResult(exec_conn->conn)) != NULL) PQclear(trail);
                    pthread_mutex_unlock(&exec_conn->mutex);

                    if (pg_stmt->result && PQresultStatus(pg_stmt->result) == PGRES_TUPLES_OK) {
                        pg_stmt->num_rows = PQntuples(pg_stmt->result);
                        pg_stmt->num_cols = PQnfields(pg_stmt->result);
                        pg_stmt->current_row = 0;
                        pg_stmt->result_conn = exec_conn;
                        pg_stmt->metadata_only_result = 0;
                        resolve_column_tables(pg_stmt, exec_conn);
                        if (pg_stmt->num_rows > 0) {
                            pthread_mutex_unlock(&pg_stmt->mutex);
                            return SQLITE_ROW;
                        }
                    } else if (pg_stmt->result) {
                        const char *err2 = PQerrorMessage(exec_conn->conn);
                        log_sql_fallback(pg_stmt->sql, pg_stmt->pg_sql, err2 ? err2 : "?", "EAGER FALLBACK");
                        PQclear(pg_stmt->result);
                        pg_stmt->result = NULL;
                        pg_pool_check_connection_health(exec_conn);
                    }
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }

                // Single-row mode activated. Connection is now exclusively ours
                // until we drain all results.
                pg_stmt->streaming_mode = 1;
                pg_stmt->streaming_conn = exec_conn;
                pg_stmt->result_conn = exec_conn;
                pg_stmt->metadata_only_result = 0;

                // Unlock connection mutex — streaming mode means we own the
                // connection exclusively (each thread has its own pool connection).
                pthread_mutex_unlock(&exec_conn->mutex);

                // Fetch first row
                PGresult *first_res = PQgetResult(exec_conn->conn);
                if (!first_res) {
                    pg_stmt->streaming_mode = 0;
                    pg_stmt->streaming_conn = NULL;
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                }

                ExecStatusType first_status = PQresultStatus(first_res);
                if (first_status == PGRES_SINGLE_TUPLE) {
                    // First row available
                    pg_stmt->result = first_res;
                    pg_stmt->current_row = 0;
                    pg_stmt->num_rows = 1;
                    pg_stmt->num_cols = PQnfields(first_res);

                    // Resolve column tables from the first row result
                    resolve_column_tables(pg_stmt, exec_conn);

                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_ROW;
                } else if (first_status == PGRES_TUPLES_OK) {
                    // Zero rows — TUPLES_OK sentinel immediately
                    PQclear(first_res);
                    PGresult *final_null = PQgetResult(exec_conn->conn);
                    if (final_null) PQclear(final_null);
                    pg_stmt->streaming_mode = 0;
                    pg_stmt->streaming_conn = NULL;
                    pg_stmt->num_cols = 0;
                    pg_stmt->num_rows = 0;
                    pg_stmt->read_done = 1;
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_DONE;
                } else {
                    // Error on first fetch
                    const char *err = PQerrorMessage(exec_conn->conn);
                    LOG_ERROR("STREAM first fetch error: %s (status=%d) sql=%.200s",
                             err ? err : "(null)", (int)first_status, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
                    PQclear(first_res);
                    PGresult *drain;
                    while ((drain = PQgetResult(exec_conn->conn)) != NULL) PQclear(drain);
                    pg_stmt->streaming_mode = 0;
                    pg_stmt->streaming_conn = NULL;
                    pg_pool_check_connection_health(exec_conn);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_ERROR;
                }
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
                // Find column indices for library_section_id and metadata_type
                // by counting commas before each column name in the INSERT column list
                const char *col_list_start = strchr(pg_stmt->pg_sql, '(');
                if (col_list_start) {
                    int lib_idx = -1, type_idx = -1;
                    const char *p = col_list_start + 1;
                    int col_idx = 0;
                    while (*p && *p != ')') {
                        // Skip whitespace
                        while (*p == ' ' || *p == '"' || *p == '`') p++;
                        // Check column name
                        if (strncmp(p, "library_section_id", 18) == 0) lib_idx = col_idx;
                        if (strncmp(p, "metadata_type", 13) == 0 &&
                            (p[13] == '"' || p[13] == '`' || p[13] == ',' || p[13] == ')' || p[13] == ' '))
                            type_idx = col_idx;
                        // Advance to next comma or end
                        while (*p && *p != ',' && *p != ')') p++;
                        if (*p == ',') { p++; col_idx++; }
                    }

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
                    return SQLITE_ERROR;
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
                return SQLITE_ERROR;
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
                        LOG_ERROR("STEP WRITE: fresh connection succeeded (reconnected)");
                    } else {
                        const char *reset_err = PQerrorMessage(new_write_conn);
                        LOG_ERROR("STEP WRITE: fresh connection also failed: %s", reset_err ? reset_err : "(null)");
                        PQfinish(new_write_conn);
                        exec_conn->is_pg_active = 0;
                        pthread_mutex_unlock(&exec_conn->mutex);
                        pg_stmt->write_executed = 1;
                        pthread_mutex_unlock(&pg_stmt->mutex);
                        return SQLITE_ERROR;
                    }
                } else {
                    LOG_ERROR("STEP WRITE: PQreset succeeded, connection recovered");
                }
            }

            // CRITICAL: Ensure connection is in blocking mode and consume any pending data
            PQsetnonblocking(exec_conn->conn, 0);
            while (PQisBusy(exec_conn->conn)) {
                PQconsumeInput(exec_conn->conn);
            }
            PGresult *pending;
            while ((pending = PQgetResult(exec_conn->conn)) != NULL) {
                LOG_ERROR("STEP WRITE: Drained orphaned result from connection %p", (void*)exec_conn);
                PQclear(pending);
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
                    } else {
                        // Prepare failed - fall back to PQexecParams
                        LOG_DEBUG("PQprepare (write) failed for %s: %s", pg_stmt->stmt_name, PQerrorMessage(exec_conn->conn));
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

int my_sqlite3_reset(sqlite3_stmt *pStmt) {
    // Clear prepared statements
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (pg_stmt) {
        pthread_mutex_lock(&pg_stmt->mutex);
        
        // CRITICAL FIX v0.9.0: Clear in_step flag to allow new bind operations
        atomic_store(&pg_stmt->in_step, 0);
        
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->param_values[i] && !is_preallocated_buffer(pg_stmt, i)) {
                free(pg_stmt->param_values[i]);
                pg_stmt->param_values[i] = NULL;
            }
        }
        pg_stmt_clear_result(pg_stmt);  // This also resets write_executed
        int is_pg_only = (pg_stmt->is_pg == 2);

        // If this is a PostgreSQL-only statement (is_pg == 2), don't call real SQLite
        // as the statement handle is not a valid SQLite statement
        if (is_pg_only) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_OK;
        }

        // CRITICAL FIX: Call orig_sqlite3_reset WHILE HOLDING THE MUTEX
        // to prevent "bind on busy prepared statement" race condition
        int rc = orig_sqlite3_reset ? orig_sqlite3_reset(pStmt) : SQLITE_ERROR;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return rc;
    }

    // Also clear cached statements - these use a separate registry
    pg_stmt_t *cached = pg_find_cached_stmt(pStmt);
    if (cached) {
        pthread_mutex_lock(&cached->mutex);
        
        // CRITICAL FIX v0.9.0: Clear in_step flag to allow new bind operations
        atomic_store(&cached->in_step, 0);
        
        pg_stmt_clear_result(cached);  // This also resets write_executed
        int is_pg_only = (cached->is_pg == 2);

        // If this is a PostgreSQL-only statement, don't call real SQLite
        if (is_pg_only) {
            pthread_mutex_unlock(&cached->mutex);
            return SQLITE_OK;
        }

        // CRITICAL FIX: Call orig_sqlite3_reset WHILE HOLDING THE MUTEX
        int rc = orig_sqlite3_reset ? orig_sqlite3_reset(pStmt) : SQLITE_ERROR;
        pthread_mutex_unlock(&cached->mutex);
        return rc;
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
            // Different statement in TLS cache (shouldn't happen, but be defensive)
            LOG_ERROR("finalize: BUG - different pg_stmt in global vs TLS for same sqlite_stmt!");
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
