#include "db_interpose_step_write_utils.h"
#include "db_interpose_common.h"
#include "db_interpose_conn_utils.h"
#include "db_interpose_txn_utils.h"
#include "db_interpose_rust.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>

pg_connection_t *step_pick_thread_connection(pg_connection_t *base_conn) {
    if (!base_conn) return NULL;
    if (!is_library_db_path(base_conn->db_path)) return base_conn;

    pg_connection_t *thread_conn = pg_get_thread_connection(base_conn->db_path);
    if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
        return thread_conn;
    }
    return base_conn;
}

int step_cached_write_should_noop(pg_connection_t *base_conn, const char *sql, pg_connection_t **out_exec_conn) {
    pg_connection_t *exec_conn = step_pick_thread_connection(base_conn);
    if (out_exec_conn) *out_exec_conn = exec_conn;
    return txn_terminator_should_noop(exec_conn, sql, NULL);
}

int step_pg_write_should_noop(pg_connection_t *exec_conn, const char *pg_sql, int *txn_state_out) {
    return txn_terminator_should_noop(exec_conn, pg_sql, txn_state_out);
}

char *step_cached_write_build_exec_sql(const char *orig_sql, const char *translated_sql, const char **exec_sql_out) {
    if (exec_sql_out) *exec_sql_out = translated_sql;
    if (!translated_sql) return NULL;

    char *owned = convert_metadata_settings_insert_to_upsert(translated_sql);
    if (owned) {
        if (exec_sql_out) *exec_sql_out = owned;
        return owned;
    }

    if (orig_sql && strncasecmp(orig_sql, "INSERT", 6) == 0 &&
        strcasestr(translated_sql, "schema_migrations") &&
        !strcasestr(translated_sql, "ON CONFLICT")) {
        size_t len = strlen(translated_sql);
        owned = malloc(len + 40);
        if (owned) {
            snprintf(owned, len + 40, "%s ON CONFLICT DO NOTHING", translated_sql);
            if (exec_sql_out) *exec_sql_out = owned;
        }
        return owned;
    }

    if (orig_sql && strncasecmp(orig_sql, "INSERT", 6) == 0 &&
        !strstr(translated_sql, "RETURNING") &&
        !strcasestr(translated_sql, "schema_migrations")) {
        size_t len = strlen(translated_sql);
        owned = malloc(len + 20);
        if (owned) {
            snprintf(owned, len + 20, "%s RETURNING id", translated_sql);
            if (exec_sql_out) *exec_sql_out = owned;
        }
    }

    return owned;
}

int step_write_should_skip_special_insert(pg_stmt_t *pg_stmt,
                                          pg_connection_t *exec_conn,
                                          const char *paramValues[MAX_PARAMS]) {
    if (!pg_stmt || !pg_stmt->pg_sql) return 0;

    if (strcasestr(pg_stmt->pg_sql, "statistics_media")) {
        const char *count_val = (pg_stmt->param_count > 6) ? paramValues[6] : NULL;
        const char *duration_val = (pg_stmt->param_count > 7) ? paramValues[7] : NULL;
        int count_empty = !count_val || strcmp(count_val, "0") == 0;
        int duration_empty = !duration_val || strcmp(duration_val, "0") == 0;

        if (count_empty && duration_empty) {
            LOG_DEBUG("SKIP statistics_media INSERT: count=%s duration=%s (empty)",
                      count_val ? count_val : "NULL", duration_val ? duration_val : "NULL");

            if (exec_conn && exec_conn->conn) {
                pthread_mutex_lock(&exec_conn->mutex);
                if (!exec_conn->conn) {
                    LOG_ERROR("SKIP SEQ: conn became NULL after lock (TOCTOU race)");
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
            return 1;
        }
    }

    if (strcasestr(pg_stmt->pg_sql, "INSERT INTO") &&
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
                return 1;
            }
        }
    }

    return 0;
}

step_result_t step_write_prepare_connection(pg_stmt_t *pg_stmt,
                                            pg_connection_t **exec_conn_io,
                                            int *pg_conn_error_out) {
    if (pg_conn_error_out) *pg_conn_error_out = 0;
    if (!pg_stmt || !exec_conn_io) return STEP_RESULT_ERROR;

    pg_connection_t *exec_conn = *exec_conn_io;

    if (!exec_conn || !exec_conn->conn) {
        LOG_ERROR("STEP WRITE: NULL connection, retrying in 500ms (exec_conn=%p)",
                  (void *)exec_conn);
        pthread_mutex_unlock(&pg_stmt->mutex);
        usleep(500000);
        pthread_mutex_lock(&pg_stmt->mutex);

        sqlite3 *retry_db = sqlite3_db_handle(pg_stmt->shadow_stmt);
        pg_connection_t *retry_handle = pg_find_connection(retry_db);
        if (retry_handle && retry_handle->db_path[0]) {
            exec_conn = pg_get_thread_connection(retry_handle->db_path);
        }
        if (!exec_conn || !exec_conn->conn) {
            LOG_ERROR("STEP WRITE: NULL connection after retry - giving up");
            pg_stmt->write_executed = 1;
            pthread_mutex_unlock(&pg_stmt->mutex);
            if (pg_conn_error_out) *pg_conn_error_out = 1;
            return STEP_RESULT_ERROR;
        }
        LOG_ERROR("STEP WRITE: reconnect retry succeeded (exec_conn=%p)", (void *)exec_conn);
    }

    pg_pool_touch_connection(exec_conn);
    pthread_mutex_lock(&exec_conn->mutex);

    if (!exec_conn->conn) {
        LOG_ERROR("STEP WRITE: conn became NULL after lock (TOCTOU race)");
        pthread_mutex_unlock(&exec_conn->mutex);
        pg_stmt->write_executed = 1;
        pthread_mutex_unlock(&pg_stmt->mutex);
        if (pg_conn_error_out) *pg_conn_error_out = 1;
        return STEP_RESULT_ERROR;
    }

    if (atomic_load(&exec_conn->streaming_active)) {
        LOG_INFO("STEP WRITE: conn %p became streaming_active after lock, getting new connection",
                 (void *)exec_conn);
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
                if (pg_conn_error_out) *pg_conn_error_out = 1;
                return STEP_RESULT_ERROR;
            }
        } else {
            LOG_ERROR("STEP WRITE: no non-streaming connection available");
            pg_stmt->write_executed = 1;
            pthread_mutex_unlock(&pg_stmt->mutex);
            if (pg_conn_error_out) *pg_conn_error_out = 1;
            return STEP_RESULT_ERROR;
        }
    }

    ConnStatusType write_conn_status = PQstatus(exec_conn->conn);
    if (write_conn_status != CONNECTION_OK) {
        const char *pg_err = PQerrorMessage(exec_conn->conn);
        LOG_ERROR("=== CONNECTION_BAD DIAGNOSTIC (WRITE) ===");
        LOG_ERROR("  Status: %d, Thread: %p", (int)write_conn_status, (void *)pthread_self());
        LOG_ERROR("  Connection: %p, PGconn: %p", (void *)exec_conn, (void *)exec_conn->conn);
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
                if (pg_conn_error_out) *pg_conn_error_out = 1;
                return STEP_RESULT_ERROR;
            }
        } else {
            LOG_ERROR("STEP WRITE: PQreset succeeded, connection recovered");
        }
    }

    step_conn_cancel_and_drain(exec_conn, "STEP WRITE");
    *exec_conn_io = exec_conn;
    return STEP_RESULT_DONE;
}

step_result_t step_write_execute_and_finalize(pg_stmt_t *pg_stmt,
                                              pg_connection_t *exec_conn,
                                              const char *paramValues[MAX_PARAMS],
                                              int *pg_conn_error_out) {
    if (pg_conn_error_out) *pg_conn_error_out = 0;
    if (!pg_stmt || !exec_conn || !exec_conn->conn) {
        if (pg_stmt) pg_stmt->write_executed = 1;
        if (pg_conn_error_out) *pg_conn_error_out = 1;
        return STEP_RESULT_ERROR;
    }

    PGresult *res = NULL;

    if (pg_stmt->use_prepared && pg_stmt->stmt_name[0]) {
        const char *cached_name = NULL;
        int is_cached = pg_stmt_cache_lookup(exec_conn, pg_stmt->sql_hash, &cached_name);

        if (!is_cached) {
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
            res = PQexecPrepared(exec_conn->conn, cached_name,
                                 pg_stmt->param_count, paramValues, NULL, NULL, 0);
        } else {
            res = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                               pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
        }
    } else {
        res = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                           pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
    }

    pthread_mutex_unlock(&exec_conn->mutex);

    ExecStatusType status = PQresultStatus(res);
    if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
        exec_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

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
                              id_str, (void *)pthread_self(), (void *)exec_conn);
                }
                sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(pg_stmt->sql);
                if (meta_id > 0) pg_set_global_metadata_id(meta_id);
            }
        }
    } else {
        const char *err = (exec_conn && exec_conn->conn) ? PQerrorMessage(exec_conn->conn) : "NULL connection";
        LOG_ERROR("STEP PG write error: %s", err);
        LOG_ERROR("  Original SQL: %.300s", pg_stmt->sql ? pg_stmt->sql : "(null)");
        LOG_ERROR("  Translated SQL: %.300s", pg_stmt->pg_sql ? pg_stmt->pg_sql : "(null)");
        if (pg_is_stale_prepared_stmt(res)) {
            pg_stmt_cache_clear_local(exec_conn);
            if (res) PQclear(res);
            if (pg_conn_error_out) *pg_conn_error_out = 1;
            pg_stmt->write_executed = 1;
            return STEP_RESULT_ERROR;
        }
        pg_pool_check_connection_health(exec_conn);
    }

    pg_stmt->write_executed = 1;
    if (res) PQclear(res);
    return STEP_RESULT_DONE;
}

step_result_t step_cached_write_execute_and_finalize(pg_stmt_t **cached_io,
                                                     sqlite3_stmt *pStmt,
                                                     pg_connection_t *changes_conn,
                                                     pg_connection_t *exec_conn,
                                                     const char *orig_sql,
                                                     const char *exec_sql,
                                                     int *pg_conn_error_out) {
    if (pg_conn_error_out) *pg_conn_error_out = 0;
    if (!exec_conn || !exec_conn->conn || !orig_sql || !exec_sql) {
        if (pg_conn_error_out) *pg_conn_error_out = 1;
        return STEP_RESULT_ERROR;
    }

    if (strstr(orig_sql, "play_queue_generators")) {
        LOG_DEBUG("CACHED INSERT play_queue_generators on thread %p conn %p",
                  (void *)pthread_self(), (void *)exec_conn);
    }

    pg_pool_touch_connection(exec_conn);
    pthread_mutex_lock(&exec_conn->mutex);

    if (!exec_conn->conn) {
        LOG_ERROR("CACHED EXEC: conn became NULL after lock (TOCTOU race)");
        pthread_mutex_unlock(&exec_conn->mutex);
        if (pg_conn_error_out) *pg_conn_error_out = 1;
        return STEP_RESULT_ERROR;
    }

    step_conn_cancel_and_drain(exec_conn, "CACHED EXEC");

    uint64_t sql_hash = pg_hash_sql(exec_sql);
    char stmt_name[32];
    snprintf(stmt_name, sizeof(stmt_name), "ce_%llx", (unsigned long long)sql_hash);

    const char *cached_stmt_name = NULL;
    PGresult *res = NULL;
    if (pg_stmt_cache_lookup(exec_conn, sql_hash, &cached_stmt_name)) {
        res = PQexecPrepared(exec_conn->conn, cached_stmt_name, 0, NULL, NULL, NULL, 0);
    } else {
        PGresult *prep_res = PQprepare(exec_conn->conn, stmt_name, exec_sql, 0, NULL);
        if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
            pg_stmt_cache_add(exec_conn, sql_hash, stmt_name, 0);
            PQclear(prep_res);
            res = PQexecPrepared(exec_conn->conn, stmt_name, 0, NULL, NULL, NULL, 0);
        } else if (pg_is_duplicate_prepared_stmt(prep_res)) {
            pg_stmt_cache_add(exec_conn, sql_hash, stmt_name, 0);
            PQclear(prep_res);
            res = PQexecPrepared(exec_conn->conn, stmt_name, 0, NULL, NULL, NULL, 0);
        } else {
            LOG_DEBUG("CACHED EXEC prepare failed, using PQexec: %s",
                      PQerrorMessage(exec_conn->conn));
            PQclear(prep_res);
            res = PQexec(exec_conn->conn, exec_sql);
        }
    }
    pthread_mutex_unlock(&exec_conn->mutex);

    ExecStatusType status = PQresultStatus(res);
    if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
        if (changes_conn) changes_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

        if (strncasecmp(orig_sql, "INSERT", 6) == 0 &&
            status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
            const char *id_str = PQgetvalue(res, 0, 0);
            if (id_str && *id_str) {
                sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(orig_sql);
                if (meta_id > 0) pg_set_global_metadata_id(meta_id);
            }
        }
    } else {
        const char *err = (changes_conn && changes_conn->conn)
                            ? PQerrorMessage(changes_conn->conn)
                            : "NULL connection";
        log_sql_fallback(orig_sql, exec_sql, err, "CACHED WRITE");
        if (pg_is_stale_prepared_stmt(res)) {
            pg_stmt_cache_clear_local(exec_conn);
            PQclear(res);
            if (pg_conn_error_out) *pg_conn_error_out = 1;
            return STEP_RESULT_ERROR;
        }
        pg_pool_check_connection_health(exec_conn);
    }

    if (res) PQclear(res);

    pg_stmt_t *cached = cached_io ? *cached_io : NULL;
    if (!cached) {
        cached = pg_stmt_create(exec_conn, orig_sql, pStmt);
        if (cached) {
            cached->is_pg = 1;
            cached->is_cached = 1;
            cached->write_executed = 1;
            pg_register_cached_stmt(pStmt, cached);
        }
        if (cached_io) *cached_io = cached;
    } else {
        cached->write_executed = 1;
    }

    return STEP_RESULT_DONE;
}

void step_write_log_debug_context(pg_stmt_t *pg_stmt,
                                  pg_connection_t *exec_conn,
                                  const char *paramValues[MAX_PARAMS]) {
    if (!pg_stmt) return;

    if (pg_stmt->pg_sql && strstr(pg_stmt->pg_sql, "play_queue_generators")) {
        LOG_DEBUG("INSERT play_queue_generators on thread %p conn %p",
                  (void *)pthread_self(), (void *)exec_conn);
    }

    if (pg_stmt->sql && strcasestr(pg_stmt->sql, "INSERT INTO metadata_items")) {
        LOG_DEBUG("STEP metadata_items INSERT: param_count=%d", pg_stmt->param_count);
        LOG_DEBUG("  PARAMS: [0]=%s [1]=%s [2]=%s [8]=%s [9]=%s",
                  (pg_stmt->param_count > 0 && paramValues[0]) ? paramValues[0] : "NULL",
                  (pg_stmt->param_count > 1 && paramValues[1]) ? paramValues[1] : "NULL",
                  (pg_stmt->param_count > 2 && paramValues[2]) ? paramValues[2] : "NULL",
                  (pg_stmt->param_count > 8 && paramValues[8]) ? paramValues[8] : "NULL",
                  (pg_stmt->param_count > 9 && paramValues[9]) ? paramValues[9] : "NULL");
    }

    if (pg_stmt->sql && strcasestr(pg_stmt->sql, "play_queue_generators")) {
        LOG_DEBUG("STEP play_queue_generators INSERT: param_count=%d", pg_stmt->param_count);
        LOG_DEBUG("  PARAMS: [0]=%s [1]=%s [2]=%s [3]=%s",
                  (pg_stmt->param_count > 0 && paramValues[0]) ? paramValues[0] : "NULL",
                  (pg_stmt->param_count > 1 && paramValues[1]) ? paramValues[1] : "NULL",
                  (pg_stmt->param_count > 2 && paramValues[2]) ? paramValues[2] : "NULL",
                  (pg_stmt->param_count > 3 && paramValues[3]) ? paramValues[3] : "NULL");
        LOG_DEBUG("  SQL: %.300s", pg_stmt->pg_sql ? pg_stmt->pg_sql : "NULL");
    }
}

void step_log_step_exit_trace(pg_stmt_t *pg_stmt) {
    if (!pg_stmt || !pg_stmt->pg_sql) return;

    int is_count = (strstr(pg_stmt->pg_sql, "COUNT(") != NULL ||
                    strstr(pg_stmt->pg_sql, "SUM(") != NULL ||
                    strstr(pg_stmt->pg_sql, "MAX(") != NULL);
    int is_playqueue = (strstr(pg_stmt->pg_sql, "play_queue") != NULL);

    if (is_count || is_playqueue) {
        LOG_DEBUG("DEBUG_TRACE: STEP_EXIT - rows=%d cols=%d sql=%.100s",
                  pg_stmt->num_rows, pg_stmt->num_cols, pg_stmt->pg_sql);
    }
}
