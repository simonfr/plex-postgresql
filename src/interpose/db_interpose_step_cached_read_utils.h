#ifndef DB_INTERPOSE_STEP_CACHED_READ_UTILS_H
#define DB_INTERPOSE_STEP_CACHED_READ_UTILS_H

#include "db_interpose.h"
#include "db_interpose_step_result.h"

int step_cached_read_maybe_advance(pg_stmt_t *cached, char *expanded_sql, step_result_t *step_rc_out);
pg_stmt_t *step_cached_read_get_or_create_stmt(pg_stmt_t *cached,
                                               pg_connection_t *conn,
                                               const char *sql,
                                               sqlite3_stmt *pStmt,
                                               const char *translated_sql);
step_result_t step_cached_read_execute_translated(pg_stmt_t *stmt,
                                                  pg_connection_t *conn,
                                                  const char *orig_sql,
                                                  const char *translated_sql,
                                                  int *pg_conn_error_out);

#endif
