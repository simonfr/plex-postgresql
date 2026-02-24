#ifndef DB_INTERPOSE_STEP_CACHED_READ_UTILS_H
#define DB_INTERPOSE_STEP_CACHED_READ_UTILS_H

#include "db_interpose.h"

int step_cached_read_maybe_advance(pg_stmt_t *cached, char *expanded_sql, int *sqlite_rc_out);
pg_stmt_t *step_cached_read_get_or_create_stmt(pg_stmt_t *cached,
                                               pg_connection_t *conn,
                                               const char *sql,
                                               sqlite3_stmt *pStmt,
                                               const char *translated_sql);

#endif
