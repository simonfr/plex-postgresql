/*
 * Plex PostgreSQL Interposing Shim - Exec Operations
 *
 * Handles sqlite3_exec function interposition.
 * 
 * Performance optimization: SQL Normalization
 * - Extracts numeric literals from SQL and converts to parameters
 * - "SELECT * FROM t WHERE id = 123" → "SELECT * FROM t WHERE id = $1" with param "123"
 * - Enables prepared statement reuse for varying SQL (huge performance win)
 * - PQexecPrepared with cached stmt: ~12µs vs PQexec: ~40µs
 */

#include "db_interpose.h"
#include "db_interpose_rust.h"
#include <ctype.h>
#include "shim_alloc.h"

#define MAX_NORMALIZED_PARAMS 32

// ============================================================================
// Exec Function - Retry Wrapper (same pattern as step retry in db_interpose_step.c)
// ============================================================================

// Thread-local flag: set to 1 by exec_impl when error is connection-related.
static __thread int exec_pg_conn_error = 0;

// Forward declaration of inner implementation
static int my_sqlite3_exec_impl(sqlite3 *db, const char *sql,
                                int (*callback)(void*, int, char**, char**),
                                void *arg, char **errmsg);

int my_sqlite3_exec(sqlite3 *db, const char *sql,
                    int (*callback)(void*, int, char**, char**),
                    void *arg, char **errmsg) {
    static __thread int exec_retry_count = 0;

    int rc = my_sqlite3_exec_impl(db, sql, callback, arg, errmsg);

    // Backoff schedule from PLEX_PG_RETRY_DELAYS (default: 500,1000,2000,3000,4000 ms)
    int exec_retry_delays_ms[PG_RETRY_MAX_DELAYS];
    int exec_max_retries = 0;
    pg_get_retry_delays(exec_retry_delays_ms, &exec_max_retries);

    if (rc == SQLITE_ERROR && exec_retry_count < exec_max_retries && exec_pg_conn_error) {
        exec_pg_conn_error = 0;
        int delay = exec_retry_delays_ms[exec_retry_count];
        exec_retry_count++;
        LOG_ERROR("exec: PG conn error, retry %d/%d in %dms (thread %p)",
                 exec_retry_count, exec_max_retries, delay, (void*)pthread_self());

        usleep(delay * 1000);
        exec_pg_conn_error = 0;
        rc = my_sqlite3_exec(db, sql, callback, arg, errmsg);  // recursive retry

        if (exec_retry_count > 0 && rc != SQLITE_ERROR) {
            LOG_ERROR("exec: retry succeeded after %d attempt(s)", exec_retry_count);
        }
        exec_retry_count = 0;
        return rc;
    }

    if (exec_retry_count > 0) {
        if (rc == SQLITE_ERROR) {
            LOG_ERROR("exec: retries exhausted, returning SQLITE_ERROR");
        }
        exec_retry_count = 0;
    }
    return rc;
}

// ============================================================================
// Exec Function - Inner Implementation
// ============================================================================

static int my_sqlite3_exec_impl(sqlite3 *db, const char *sql,
                                int (*callback)(void*, int, char**, char**),
                                void *arg, char **errmsg) {
    // CRITICAL FIX: NULL check to prevent crash in strcasestr
    if (!sql) {
        LOG_ERROR("exec called with NULL SQL");
        return orig_sqlite3_exec ? orig_sqlite3_exec(db, sql, callback, arg, errmsg) : SQLITE_ERROR;
    }

    pg_connection_t *pg_conn = pg_find_connection(db);

    if (pg_conn && pg_conn->is_pg_active) {
        // Pre-flight connection health check (mirrors step_impl pattern)
        if (!pg_conn->conn || PQstatus(pg_conn->conn) != CONNECTION_OK) {
            LOG_ERROR("EXEC: CONNECTION_BAD pre-flight, attempting reconnect (thread %p)",
                     (void*)pthread_self());
            pthread_mutex_lock(&pg_conn->mutex);
            if (pg_conn->conn) {
                PQreset(pg_conn->conn);
                if (PQstatus(pg_conn->conn) != CONNECTION_OK) {
                    LOG_ERROR("EXEC: PQreset failed, trying fresh PQconnectdb...");
                    pg_stmt_cache_clear(pg_conn);
                    PQfinish(pg_conn->conn);
                    pg_conn->conn = NULL;

                    pg_conn_config_t *rcfg = pg_config_get();
                    char rconninfo[1024];
                    snprintf(rconninfo, sizeof(rconninfo),
                             "host=%s port=%d dbname=%s user=%s password=%s "
                             "connect_timeout=5 keepalives=1 keepalives_idle=30 "
                             "keepalives_interval=10 keepalives_count=3",
                             rcfg->host, rcfg->port, rcfg->database, rcfg->user, rcfg->password);
                    PGconn *new_conn = PQconnectdb(rconninfo);
                    if (PQstatus(new_conn) == CONNECTION_OK) {
                        pg_conn->conn = new_conn;
                        pg_conn->is_pg_active = 1;
                        LOG_INFO("EXEC: fresh connection succeeded (reconnected)");
                    } else {
                        LOG_ERROR("EXEC: fresh connection also failed: %s",
                                 PQerrorMessage(new_conn));
                        PQfinish(new_conn);
                        pg_conn->is_pg_active = 0;
                        pthread_mutex_unlock(&pg_conn->mutex);
                        exec_pg_conn_error = 1;
                        return SQLITE_ERROR;
                    }
                } else {
                    LOG_ERROR("EXEC: PQreset succeeded, connection recovered");
                }
                // Re-apply search_path and statement_timeout after reconnect
                pg_conn_config_t *cfg = pg_config_get();
                if (cfg) {
                    char schema_cmd[256];
                    snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
                    PGresult *r = PQexec(pg_conn->conn, schema_cmd);
                    PQclear(r);
                    r = PQexec(pg_conn->conn, "SET statement_timeout = '5min'");
                    PQclear(r);
                }
            } else {
                // conn is NULL, try fresh connection
                pg_conn_config_t *rcfg = pg_config_get();
                char rconninfo[1024];
                snprintf(rconninfo, sizeof(rconninfo),
                         "host=%s port=%d dbname=%s user=%s password=%s "
                         "connect_timeout=5 keepalives=1 keepalives_idle=30 "
                         "keepalives_interval=10 keepalives_count=3",
                         rcfg->host, rcfg->port, rcfg->database, rcfg->user, rcfg->password);
                PGconn *new_conn = PQconnectdb(rconninfo);
                if (PQstatus(new_conn) == CONNECTION_OK) {
                    pg_conn->conn = new_conn;
                    pg_conn->is_pg_active = 1;
                    LOG_ERROR("EXEC: fresh connection from NULL succeeded");
                    pg_conn_config_t *cfg = pg_config_get();
                    if (cfg) {
                        char schema_cmd[256];
                        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
                        PGresult *r = PQexec(pg_conn->conn, schema_cmd);
                        PQclear(r);
                        r = PQexec(pg_conn->conn, "SET statement_timeout = '5min'");
                        PQclear(r);
                    }
                } else {
                    LOG_ERROR("EXEC: fresh connection from NULL failed: %s",
                             PQerrorMessage(new_conn));
                    PQfinish(new_conn);
                    pg_conn->is_pg_active = 0;
                    pthread_mutex_unlock(&pg_conn->mutex);
                    exec_pg_conn_error = 1;
                    return SQLITE_ERROR;
                }
            }
            pthread_mutex_unlock(&pg_conn->mutex);
        }

        // Rewrite schema_migrations for blobs.db connections
        char *blobs_rewrite = rewrite_blobs_schema_migrations(sql, pg_conn->db_path);
        if (blobs_rewrite) sql = blobs_rewrite;

        if (!should_skip_sql(sql)) {
            // GUARD: Block junk INSERTs into metadata_items with NULL library_section_id AND metadata_type
            if (rust_is_junk_metadata_insert(sql)) {
                LOG_ERROR("GUARD: Blocked exec junk INSERT into metadata_items "
                          "(library_section_id=NULL, metadata_type=NULL)");
                return SQLITE_OK;
            }

            sql_translation_t trans = sql_translate(sql);
            if (trans.success && trans.sql) {
                char *exec_sql = trans.sql;
                char *insert_sql = NULL;

                // Add RETURNING id for INSERT statements
                if (strncasecmp(sql, "INSERT", 6) == 0 && !strstr(trans.sql, "RETURNING")) {
                    size_t len = strlen(trans.sql);
                    insert_sql = malloc(len + 20);
                    if (insert_sql) {
                        snprintf(insert_sql, len + 20, "%s RETURNING id", trans.sql);
                        exec_sql = insert_sql;
                        if (strstr(sql, "play_queue_generators")) {
                            LOG_INFO("EXEC play_queue_generators INSERT with RETURNING: %s", exec_sql);
                        }
                    }
                }

                // CRITICAL FIX: Lock connection mutex to prevent concurrent libpq access
                pthread_mutex_lock(&pg_conn->mutex);
                
                PGresult *res = NULL;
                
                // PERFORMANCE OPTIMIZATION: SQL Normalization
                // Try to extract numeric literals as parameters for prepared statement reuse
                // "WHERE id = 123" → "WHERE id = $1" with param "123"
                normalized_sql_t *normalized = rust_normalize_sql_literals(exec_sql);
                
                if (normalized) {
                    // Normalization succeeded - use prepared statement with extracted params
                    uint64_t norm_hash = pg_hash_sql(normalized->normalized_sql);
                    const char *cached_stmt_name = NULL;
                    char stmt_name[32];
                    
                    if (pg_stmt_cache_lookup(pg_conn, norm_hash, &cached_stmt_name)) {
                        // Cache HIT - execute with extracted parameters
                        const char *param_ptrs[MAX_NORMALIZED_PARAMS];
                        for (int i = 0; i < normalized->param_count; i++) {
                            param_ptrs[i] = normalized->param_values[i];
                        }
                        res = PQexecPrepared(pg_conn->conn, cached_stmt_name, 
                                            normalized->param_count, param_ptrs, NULL, NULL, 0);
                    } else {
                        // Cache MISS - prepare normalized SQL, then execute
                        snprintf(stmt_name, sizeof(stmt_name), "nx_%llx", (unsigned long long)norm_hash);
                        PGresult *prep_res = PQprepare(pg_conn->conn, stmt_name, 
                                                       normalized->normalized_sql, 0, NULL);
                        if (PQresultStatus(prep_res) == PGRES_COMMAND_OK ||
                            pg_is_duplicate_prepared_stmt(prep_res)) {
                            pg_stmt_cache_add(pg_conn, norm_hash, stmt_name, normalized->param_count);
                            PQclear(prep_res);
                            
                            const char *param_ptrs[MAX_NORMALIZED_PARAMS];
                            for (int i = 0; i < normalized->param_count; i++) {
                                param_ptrs[i] = normalized->param_values[i];
                            }
                            res = PQexecPrepared(pg_conn->conn, stmt_name,
                                                normalized->param_count, param_ptrs, NULL, NULL, 0);
                        } else {
                            // Prepare failed - fall back to direct exec
                            PQclear(prep_res);
                            res = PQexec(pg_conn->conn, exec_sql);
                        }
                    }
                    rust_free_normalized_sql(normalized);
                } else {
                    // Normalization not applicable - try regular prepared stmt cache or direct exec
                    uint64_t sql_hash = pg_hash_sql(exec_sql);
                    const char *cached_stmt_name = NULL;
                    
                    if (pg_stmt_cache_lookup(pg_conn, sql_hash, &cached_stmt_name)) {
                        // Cache HIT for exact SQL match
                        res = PQexecPrepared(pg_conn->conn, cached_stmt_name, 0, NULL, NULL, NULL, 0);
                    } else {
                        // Cache MISS - use PQexec directly (1 round-trip)
                        res = PQexec(pg_conn->conn, exec_sql);
                    }
                }
                
                ExecStatusType status = PQresultStatus(res);

                if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
                    pg_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

                    // Extract ID from RETURNING clause for INSERT
                    if (strncasecmp(sql, "INSERT", 6) == 0 && status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                        const char *id_str = PQgetvalue(res, 0, 0);
                        if (id_str && *id_str) {
                            if (strstr(sql, "play_queue_generators")) {
                                LOG_INFO("EXEC play_queue_generators: RETURNING id = %s", id_str);
                            }
                            sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(sql);
                            if (meta_id > 0) pg_set_global_metadata_id(meta_id);
                        }
                    }
                } else {
                    const char *err = (pg_conn && pg_conn->conn) ? PQerrorMessage(pg_conn->conn) : "NULL connection";
                    LOG_ERROR("PostgreSQL exec error: %s", err);
                    // Check if this is a connection error or stale prepared statement
                    int is_conn_error = (!pg_conn->conn || PQstatus(pg_conn->conn) != CONNECTION_OK);
                    // v0.9.38: Stale prepared statement recovery (SQLSTATE 26000)
                    int is_stale_stmt = pg_is_stale_prepared_stmt(res);
                    if (is_stale_stmt) pg_stmt_cache_clear_local(pg_conn);
                    pg_pool_check_connection_health(pg_conn);
                    if (is_conn_error || is_stale_stmt) {
                        if (insert_sql) free(insert_sql);
                        PQclear(res);
                        pthread_mutex_unlock(&pg_conn->mutex);
                        sql_translation_free(&trans);
                        if (blobs_rewrite) free(blobs_rewrite);
                        exec_pg_conn_error = 1;
                        return SQLITE_ERROR;
                    }
                }

                if (insert_sql) free(insert_sql);
                PQclear(res);
                pthread_mutex_unlock(&pg_conn->mutex);
            }
            sql_translation_free(&trans);
        }
        if (blobs_rewrite) free(blobs_rewrite);
        return SQLITE_OK;
    }

    // For non-PG databases, strip collate icu_root since SQLite doesn't support it
    char *cleaned_sql = NULL;
    const char *exec_sql = sql;
    if (strcasestr(sql, "collate icu_root")) {
        cleaned_sql = rust_strip_collate_icu_root(sql);
        if (cleaned_sql) exec_sql = cleaned_sql;
    }

    int rc = orig_sqlite3_exec ? orig_sqlite3_exec(db, exec_sql, callback, arg, errmsg) : SQLITE_ERROR;
    if (cleaned_sql) rust_free_cstring(cleaned_sql);
    return rc;
}
