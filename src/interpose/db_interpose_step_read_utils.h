#ifndef DB_INTERPOSE_STEP_READ_UTILS_H
#define DB_INTERPOSE_STEP_READ_UTILS_H

#include "db_interpose.h"

int step_read_advance_cached_result(pg_stmt_t *stmt);
int step_read_streaming_next(sqlite3_stmt *pStmt, pg_stmt_t *stmt);
int step_read_eager_next(pg_stmt_t *stmt);
int step_read_first_execute(pg_stmt_t *stmt,
                            pg_connection_t **exec_conn_io,
                            const char *paramValues[MAX_PARAMS],
                            int *pg_conn_error_out);

#endif
