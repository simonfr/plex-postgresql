#ifndef DB_INTERPOSE_STEP_WRITE_UTILS_H
#define DB_INTERPOSE_STEP_WRITE_UTILS_H

#include "db_interpose.h"

pg_connection_t *step_pick_thread_connection(pg_connection_t *base_conn);
int step_cached_write_should_noop(pg_connection_t *base_conn, const char *sql, pg_connection_t **out_exec_conn);
int step_pg_write_should_noop(pg_connection_t *exec_conn, const char *pg_sql, int *txn_state_out);
char *step_cached_write_build_exec_sql(const char *orig_sql, const char *translated_sql, const char **exec_sql_out);
int step_write_should_skip_special_insert(pg_stmt_t *pg_stmt,
                                          pg_connection_t *exec_conn,
                                          const char *paramValues[MAX_PARAMS]);

#endif
