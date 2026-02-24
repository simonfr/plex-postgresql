#include "db_interpose_step_cached_read_utils.h"
#include <stdio.h>
#include <string.h>
#include <strings.h>

int step_cached_read_maybe_advance(pg_stmt_t *cached, char *expanded_sql, step_result_t *step_rc_out) {
    if (step_rc_out) *step_rc_out = STEP_RESULT_DONE;
    if (!cached || !cached->result) return 0;

    cached->current_row++;
    if (cached->current_row >= cached->num_rows) {
        // Free PGresult immediately when done; Plex may not call reset().
        PQclear(cached->result);
        cached->result = NULL;
        if (expanded_sql) sqlite3_free(expanded_sql);
        if (step_rc_out) *step_rc_out = STEP_RESULT_DONE;
        return 1;
    }

    if (expanded_sql) sqlite3_free(expanded_sql);
    if (step_rc_out) *step_rc_out = STEP_RESULT_ROW;
    return 1;
}

pg_stmt_t *step_cached_read_get_or_create_stmt(pg_stmt_t *cached,
                                               pg_connection_t *conn,
                                               const char *sql,
                                               sqlite3_stmt *pStmt,
                                               const char *translated_sql) {
    if (cached) return cached;
    if (!conn || !sql || !pStmt || !translated_sql) return NULL;

    pg_stmt_t *new_stmt = pg_stmt_create(conn, sql, pStmt);
    if (!new_stmt) return NULL;

    new_stmt->pg_sql = strdup(translated_sql);
    new_stmt->is_pg = 2;
    new_stmt->is_cached = 1;
    pg_register_cached_stmt(pStmt, new_stmt);
    return new_stmt;
}

step_result_t step_cached_read_execute_translated(pg_stmt_t *stmt,
                                                  pg_connection_t *conn,
                                                  const char *orig_sql,
                                                  const char *translated_sql,
                                                  int *pg_conn_error_out) {
    if (pg_conn_error_out) *pg_conn_error_out = 0;
    if (!stmt || !conn || !conn->conn || !translated_sql) return STEP_RESULT_FALLBACK;

    pg_pool_touch_connection(conn);
    pthread_mutex_lock(&conn->mutex);

    if (!conn->conn) {
        LOG_ERROR("CACHED READ: conn became NULL after lock (TOCTOU race)");
        pthread_mutex_unlock(&conn->mutex);
        if (pg_conn_error_out) *pg_conn_error_out = 1;
        return STEP_RESULT_ERROR;
    }

    if (!atomic_load(&conn->streaming_active)) {
        PQsetnonblocking(conn->conn, 0);
        while (PQisBusy(conn->conn)) {
            PQconsumeInput(conn->conn);
        }

        PGcancel *cr_cancel = PQgetCancel(conn->conn);
        if (cr_cancel) {
            char cr_errbuf[256];
            PQcancel(cr_cancel, cr_errbuf, sizeof(cr_errbuf));
            PQfreeCancel(cr_cancel);
        }

        PGresult *pending_read;
        int drain_cr = 0;
        while ((pending_read = PQgetResult(conn->conn)) != NULL) {
            drain_cr++;
            if (drain_cr <= 3) {
                LOG_INFO("CACHED READ: Drained orphaned result from connection %p (status=%d)",
                         (void *)conn, PQresultStatus(pending_read));
            }
            PQclear(pending_read);
            if (drain_cr > 1000) {
                LOG_INFO("CACHED READ: Drain loop exceeded 1000 on %p — aborting drain", (void *)conn);
                break;
            }
        }
    }

    uint64_t read_sql_hash = pg_hash_sql(translated_sql);
    char read_stmt_name[32];
    snprintf(read_stmt_name, sizeof(read_stmt_name), "cr_%llx", (unsigned long long)read_sql_hash);
    if (strcasestr(translated_sql, "DISTINCT")) {
        LOG_ERROR("TRACE_STEP_PGSQL hash=0x%llx sql=%.1200s",
                  (unsigned long long)read_sql_hash,
                  translated_sql);
    }

    const char *cached_read_stmt_name = NULL;
    if (pg_stmt_cache_lookup(conn, read_sql_hash, &cached_read_stmt_name)) {
        LOG_DEBUG("CACHED READ (prepared): stmt=%s sql=%.60s", cached_read_stmt_name, translated_sql);
        stmt->result = PQexecPrepared(conn->conn, cached_read_stmt_name, 0, NULL, NULL, NULL, 0);
    } else {
        PGresult *prep_res = PQprepare(conn->conn, read_stmt_name, translated_sql, 0, NULL);
        if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
            pg_stmt_cache_add(conn, read_sql_hash, read_stmt_name, 0);
            PQclear(prep_res);
            LOG_DEBUG("CACHED READ (new prepared): stmt=%s sql=%.60s", read_stmt_name, translated_sql);
            stmt->result = PQexecPrepared(conn->conn, read_stmt_name, 0, NULL, NULL, NULL, 0);
        } else if (pg_is_duplicate_prepared_stmt(prep_res)) {
            pg_stmt_cache_add(conn, read_sql_hash, read_stmt_name, 0);
            PQclear(prep_res);
            stmt->result = PQexecPrepared(conn->conn, read_stmt_name, 0, NULL, NULL, NULL, 0);
        } else {
            LOG_DEBUG("CACHED READ prepare failed, using PQexec: %s",
                      PQerrorMessage(conn->conn));
            PQclear(prep_res);
            stmt->result = PQexec(conn->conn, translated_sql);
        }
    }
    pthread_mutex_unlock(&conn->mutex);

    if (PQresultStatus(stmt->result) == PGRES_TUPLES_OK) {
        stmt->num_rows = PQntuples(stmt->result);
        stmt->num_cols = PQnfields(stmt->result);
        stmt->current_row = 0;
        stmt->result_conn = conn;

        if (resolve_column_tables(stmt, conn) < 0) {
            LOG_ERROR("Failed to resolve column tables, cleaning up");
        }
        return (stmt->num_rows > 0) ? STEP_RESULT_ROW : STEP_RESULT_DONE;
    }

    const char *err = (conn && conn->conn) ? PQerrorMessage(conn->conn) : "NULL connection";
    log_sql_fallback(orig_sql, translated_sql, err, "CACHED READ");
    if (pg_is_stale_prepared_stmt(stmt->result)) {
        pg_stmt_cache_clear_local(conn);
        PQclear(stmt->result);
        stmt->result = NULL;
        if (pg_conn_error_out) *pg_conn_error_out = 1;
        return STEP_RESULT_ERROR;
    }
    PQclear(stmt->result);
    stmt->result = NULL;
    pg_pool_check_connection_health(conn);
    return STEP_RESULT_FALLBACK;
}
