#include "db_interpose_step_read_utils.h"
#include "db_interpose_common.h"
#include "pg_query_cache.h"

static void step_read_clear_row_caches(pg_stmt_t *stmt) {
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->cached_text[i]) {
            free(stmt->cached_text[i]);
            stmt->cached_text[i] = NULL;
        }
        if (stmt->cached_blob[i]) {
            free(stmt->cached_blob[i]);
            stmt->cached_blob[i] = NULL;
            stmt->cached_blob_len[i] = 0;
        }
        if (stmt->decoded_blobs[i]) {
            free(stmt->decoded_blobs[i]);
            stmt->decoded_blobs[i] = NULL;
            stmt->decoded_blob_lens[i] = 0;
        }
    }
    stmt->cached_row = -1;
    stmt->decoded_blob_row = -1;
}

step_result_t step_read_advance_cached_result(pg_stmt_t *stmt) {
    if (!stmt || !stmt->cached_result) return STEP_RESULT_ERROR;

    stmt->current_row++;
    if (stmt->current_row >= stmt->num_rows) {
        pg_query_cache_release(stmt->cached_result);
        stmt->cached_result = NULL;
        stmt->read_done = 1;
        pthread_mutex_unlock(&stmt->mutex);
        return STEP_RESULT_DONE;
    }
    pthread_mutex_unlock(&stmt->mutex);
    return STEP_RESULT_ROW;
}

step_result_t step_read_streaming_next(sqlite3_stmt *pStmt, pg_stmt_t *stmt) {
    if (!stmt || !stmt->streaming_mode || !stmt->streaming_conn || !stmt->streaming_conn->conn) {
        return STEP_RESULT_ERROR;
    }

    if (stmt->result) {
        PQclear(stmt->result);
        stmt->result = NULL;
    }
    step_read_clear_row_caches(stmt);

    PGresult *row_res = PQgetResult(stmt->streaming_conn->conn);
    if (!row_res) {
        LOG_ERROR("STREAM: NULL result (unexpected!) stmt=%p sql=%.100s streaming_conn=%p",
                  (void *)pStmt, stmt->pg_sql ? stmt->pg_sql : "?",
                  (void *)stmt->streaming_conn);
        stmt->streaming_mode = 0;
        if (stmt->streaming_conn) atomic_store(&stmt->streaming_conn->streaming_active, 0);
        stmt->streaming_conn = NULL;
        stmt->read_done = 1;
        pthread_mutex_unlock(&stmt->mutex);
        return STEP_RESULT_DONE;
    }

    ExecStatusType row_status = PQresultStatus(row_res);
    if (row_status == PGRES_SINGLE_TUPLE) {
        stmt->result = row_res;
        stmt->current_row = 0;
        stmt->num_rows = 1;
        stmt->num_cols = PQnfields(row_res);
        pthread_mutex_unlock(&stmt->mutex);
        return STEP_RESULT_ROW;
    }
    if (row_status == PGRES_TUPLES_OK) {
        PQclear(row_res);
        PGresult *final_null = PQgetResult(stmt->streaming_conn->conn);
        if (final_null) PQclear(final_null);
        stmt->streaming_mode = 0;
        if (stmt->streaming_conn) atomic_store(&stmt->streaming_conn->streaming_active, 0);
        stmt->streaming_conn = NULL;
        stmt->read_done = 1;
        pthread_mutex_unlock(&stmt->mutex);
        return STEP_RESULT_DONE;
    }

    const char *err = PQerrorMessage(stmt->streaming_conn->conn);
    LOG_ERROR("STREAM ERROR: %s (status=%d) sql=%.100s",
              err ? err : "(null)", (int)row_status, stmt->pg_sql ? stmt->pg_sql : "?");
    PQclear(row_res);
    PGresult *drain;
    while ((drain = PQgetResult(stmt->streaming_conn->conn)) != NULL) PQclear(drain);
    stmt->streaming_mode = 0;
    if (stmt->streaming_conn) atomic_store(&stmt->streaming_conn->streaming_active, 0);
    stmt->streaming_conn = NULL;
    stmt->read_done = 1;
    pthread_mutex_unlock(&stmt->mutex);
    return STEP_RESULT_DONE;
}

step_result_t step_read_eager_next(pg_stmt_t *stmt) {
    if (!stmt || !stmt->result) return STEP_RESULT_ERROR;

    stmt->current_row++;
    if (stmt->current_row >= stmt->num_rows) {
        PQclear(stmt->result);
        stmt->result = NULL;
        stmt->result_conn = NULL;
        stmt->read_done = 1;
        pthread_mutex_unlock(&stmt->mutex);
        return STEP_RESULT_DONE;
    }
    pthread_mutex_unlock(&stmt->mutex);
    return STEP_RESULT_ROW;
}

step_result_t step_read_first_execute(pg_stmt_t *pg_stmt,
                                      pg_connection_t **exec_conn_io,
                                      const char *paramValues[MAX_PARAMS],
                                      int *pg_conn_error_out) {
    if (pg_conn_error_out) *pg_conn_error_out = 0;
    if (!pg_stmt || !exec_conn_io) return STEP_RESULT_ERROR;

    pg_connection_t *exec_conn = *exec_conn_io;
    pthread_t current = pthread_self();
    pg_stmt->executing_thread = current;

    if (!exec_conn || !exec_conn->conn) {
        LOG_ERROR("STEP SELECT: NULL connection, retrying in 500ms (exec_conn=%p)",
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
            LOG_ERROR("STEP SELECT: NULL connection after retry — giving up");
            pthread_mutex_unlock(&pg_stmt->mutex);
            if (pg_conn_error_out) *pg_conn_error_out = 1;
            return STEP_RESULT_ERROR;
        }
        LOG_ERROR("STEP SELECT: reconnect retry succeeded (exec_conn=%p)", (void *)exec_conn);
    }

    pg_pool_touch_connection(exec_conn);
    pthread_mutex_lock(&exec_conn->mutex);

    if (!exec_conn->conn) {
        LOG_ERROR("STEP SELECT: conn became NULL after lock (TOCTOU race)");
        pthread_mutex_unlock(&exec_conn->mutex);
        pthread_mutex_unlock(&pg_stmt->mutex);
        if (pg_conn_error_out) *pg_conn_error_out = 1;
        return STEP_RESULT_ERROR;
    }

    if (atomic_load(&exec_conn->streaming_active)) {
        LOG_INFO("STEP SELECT: conn %p became streaming_active after lock, getting new connection",
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
                LOG_ERROR("STEP SELECT: alt conn also unavailable");
                pthread_mutex_unlock(&exec_conn->mutex);
                pthread_mutex_unlock(&pg_stmt->mutex);
                if (pg_conn_error_out) *pg_conn_error_out = 1;
                return STEP_RESULT_ERROR;
            }
        } else {
            LOG_ERROR("STEP SELECT: no non-streaming connection available");
            pthread_mutex_unlock(&pg_stmt->mutex);
            if (pg_conn_error_out) *pg_conn_error_out = 1;
            return STEP_RESULT_ERROR;
        }
    }

    ConnStatusType conn_status = PQstatus(exec_conn->conn);
    if (conn_status != CONNECTION_OK) {
        const char *pg_err = PQerrorMessage(exec_conn->conn);
        LOG_ERROR("=== CONNECTION_BAD DIAGNOSTIC (READ) ===");
        LOG_ERROR("  Status: %d, Thread: %p", (int)conn_status, (void *)pthread_self());
        LOG_ERROR("  Connection: %p, PGconn: %p", (void *)exec_conn, (void *)exec_conn->conn);
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
                LOG_INFO("STEP READ: fresh connection succeeded (reconnected)");
            } else {
                const char *reset_err = PQerrorMessage(new_read_conn);
                LOG_ERROR("STEP READ: fresh connection also failed: %s", reset_err ? reset_err : "(null)");
                PQfinish(new_read_conn);
                exec_conn->is_pg_active = 0;
                pthread_mutex_unlock(&exec_conn->mutex);
                pthread_mutex_unlock(&pg_stmt->mutex);
                if (pg_conn_error_out) *pg_conn_error_out = 1;
                return STEP_RESULT_ERROR;
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

    if (!atomic_load(&exec_conn->streaming_active)) {
        PQsetnonblocking(exec_conn->conn, 0);
        while (PQisBusy(exec_conn->conn)) {
            PQconsumeInput(exec_conn->conn);
        }
        PGcancel *rd_cancel = PQgetCancel(exec_conn->conn);
        if (rd_cancel) {
            char rd_errbuf[256];
            PQcancel(rd_cancel, rd_errbuf, sizeof(rd_errbuf));
            PQfreeCancel(rd_cancel);
        }
        PGresult *pending;
        int drain_count_r = 0;
        while ((pending = PQgetResult(exec_conn->conn)) != NULL) {
            drain_count_r++;
            if (drain_count_r <= 3) {
                LOG_INFO("STEP READ: Drained orphaned result from connection %p (status=%d: %s)",
                         (void *)exec_conn, PQresultStatus(pending),
                         PQresStatus(PQresultStatus(pending)));
            }
            PQclear(pending);
            if (drain_count_r > 1000) {
                LOG_INFO("STEP READ: Drain loop exceeded 1000 on %p — aborting drain", (void *)exec_conn);
                break;
            }
        }
        if (drain_count_r > 3) {
            LOG_INFO("STEP READ: Drained %d orphaned results total from connection %p", drain_count_r, (void *)exec_conn);
        }
    }

    {
        PGresult *to_res = PQexec(exec_conn->conn, "SET statement_timeout = '5min'");
        PQclear(to_res);
    }

    int send_ok = 0;
    if (pg_stmt->pg_sql) {
        pg_exception_note_query(pg_stmt->pg_sql);
    }

    LOG_DEBUG("PREPARED CHECK: use_prepared=%d stmt_name[0]=%d pg_sql=%p",
              pg_stmt->use_prepared, (int)pg_stmt->stmt_name[0], (void *)pg_stmt->pg_sql);
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
            } else if (pg_is_duplicate_prepared_stmt(prep_res)) {
                pg_stmt_cache_add(exec_conn, pg_stmt->sql_hash, pg_stmt->stmt_name, pg_stmt->param_count);
                cached_name = pg_stmt->stmt_name;
                is_cached = 1;
            } else {
                LOG_ERROR("PQprepare failed for %s: %s", pg_stmt->stmt_name,
                          PQerrorMessage(exec_conn->conn));
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
        if (err && strstr(err, "does not exist")) {
            pg_stmt_cache_clear_local(exec_conn);
        }
        pthread_mutex_unlock(&exec_conn->mutex);
        pg_pool_check_connection_health(exec_conn);
        pthread_mutex_unlock(&pg_stmt->mutex);
        if (pg_conn_error_out) *pg_conn_error_out = 1;
        return STEP_RESULT_ERROR;
    }

    if (!PQsetSingleRowMode(exec_conn->conn)) {
        LOG_ERROR("PQsetSingleRowMode failed, falling back to eager fetch");
        pg_stmt->result = PQgetResult(exec_conn->conn);
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
                *exec_conn_io = exec_conn;
                pthread_mutex_unlock(&pg_stmt->mutex);
                return STEP_RESULT_ROW;
            }
        } else if (pg_stmt->result) {
            const char *err2 = PQerrorMessage(exec_conn->conn);
            log_sql_fallback(pg_stmt->sql, pg_stmt->pg_sql, err2 ? err2 : "?", "EAGER FALLBACK");
            PQclear(pg_stmt->result);
            pg_stmt->result = NULL;
            pg_pool_check_connection_health(exec_conn);
        }
        pg_stmt->read_done = 1;
        *exec_conn_io = exec_conn;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return STEP_RESULT_DONE;
    }

    pg_stmt->streaming_mode = 1;
    pg_stmt->streaming_conn = exec_conn;
    pg_stmt->result_conn = exec_conn;
    atomic_store(&exec_conn->streaming_active, 1);
    pg_stmt->metadata_only_result = 0;
    pthread_mutex_unlock(&exec_conn->mutex);

    PGresult *first_res = PQgetResult(exec_conn->conn);
    if (!first_res) {
        pg_stmt->streaming_mode = 0;
        if (pg_stmt->streaming_conn) atomic_store(&pg_stmt->streaming_conn->streaming_active, 0);
        pg_stmt->streaming_conn = NULL;
        pg_stmt->read_done = 1;
        *exec_conn_io = exec_conn;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return STEP_RESULT_DONE;
    }

    ExecStatusType first_status = PQresultStatus(first_res);
    if (first_status == PGRES_SINGLE_TUPLE) {
        pg_stmt->result = first_res;
        pg_stmt->current_row = 0;
        pg_stmt->num_rows = 1;
        pg_stmt->num_cols = PQnfields(first_res);
        resolve_column_tables(pg_stmt, exec_conn);
        *exec_conn_io = exec_conn;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return STEP_RESULT_ROW;
    }
    if (first_status == PGRES_TUPLES_OK) {
        LOG_DEBUG("STREAM: zero rows returned for sql=%.200s",
                  pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
        PQclear(first_res);
        PGresult *final_null = PQgetResult(exec_conn->conn);
        if (final_null) PQclear(final_null);
        pg_stmt->streaming_mode = 0;
        if (pg_stmt->streaming_conn) atomic_store(&pg_stmt->streaming_conn->streaming_active, 0);
        pg_stmt->streaming_conn = NULL;
        pg_stmt->num_cols = 0;
        pg_stmt->num_rows = 0;
        pg_stmt->read_done = 1;
        *exec_conn_io = exec_conn;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return STEP_RESULT_DONE;
    }

    const char *err = PQerrorMessage(exec_conn->conn);
    LOG_ERROR("STREAM first fetch error: %s (status=%d) sql=%.200s",
              err ? err : "(null)", (int)first_status, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
    if (pg_is_stale_prepared_stmt(first_res)) {
        pg_stmt_cache_clear_local(exec_conn);
    }
    PQclear(first_res);
    PGresult *drain;
    while ((drain = PQgetResult(exec_conn->conn)) != NULL) PQclear(drain);
    pg_stmt->streaming_mode = 0;
    if (pg_stmt->streaming_conn) atomic_store(&pg_stmt->streaming_conn->streaming_active, 0);
    pg_stmt->streaming_conn = NULL;
    pg_pool_check_connection_health(exec_conn);
    *exec_conn_io = exec_conn;
    pthread_mutex_unlock(&pg_stmt->mutex);
    if (pg_conn_error_out) *pg_conn_error_out = 1;
    return STEP_RESULT_ERROR;
}
