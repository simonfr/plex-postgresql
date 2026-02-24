#include "db_interpose_step_write_utils.h"
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
