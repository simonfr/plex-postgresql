#include "db_interpose_step_cached_read_utils.h"
#include <string.h>

int step_cached_read_maybe_advance(pg_stmt_t *cached, char *expanded_sql, int *sqlite_rc_out) {
    if (sqlite_rc_out) *sqlite_rc_out = SQLITE_DONE;
    if (!cached || !cached->result) return 0;

    cached->current_row++;
    if (cached->current_row >= cached->num_rows) {
        // Free PGresult immediately when done; Plex may not call reset().
        PQclear(cached->result);
        cached->result = NULL;
        if (expanded_sql) sqlite3_free(expanded_sql);
        if (sqlite_rc_out) *sqlite_rc_out = SQLITE_DONE;
        return 1;
    }

    if (expanded_sql) sqlite3_free(expanded_sql);
    if (sqlite_rc_out) *sqlite_rc_out = SQLITE_ROW;
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
