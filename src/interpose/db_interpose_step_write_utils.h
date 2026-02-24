#ifndef DB_INTERPOSE_STEP_WRITE_UTILS_H
#define DB_INTERPOSE_STEP_WRITE_UTILS_H

#include "db_interpose.h"
#include "db_interpose_step_result.h"

/*
 * Contract:
 * - For prepare/execute helpers, caller holds pg_stmt->mutex on entry.
 * - Helpers may lock/unlock exec_conn->mutex internally.
 * - Some helpers set write_executed as part of terminal handling.
 * - STEP_RESULT_ERROR + conn_error_out=1 means connection-level failure (retry-eligible).
 */

/* === prepare === */

pg_connection_t *step_pick_thread_connection(pg_connection_t *base_conn);
int step_cached_write_should_noop(pg_connection_t *base_conn, const char *sql, pg_connection_t **out_exec_conn);
int step_pg_write_should_noop(pg_connection_t *exec_conn, const char *pg_sql, int *txn_state_out);
char *step_cached_write_build_exec_sql(const char *orig_sql, const char *translated_sql, const char **exec_sql_out);

/* Prepare write connection for execution (touch/lock/recover/drain). */
step_result_t step_write_prepare_connection(pg_stmt_t *pg_stmt,
                                            pg_connection_t **exec_conn_io,
                                            int *pg_conn_error_out);

/* === policy === */

/* Policy guards for known bad/special inserts; may set write_executed. */
int step_write_should_skip_special_insert(pg_stmt_t *pg_stmt,
                                          pg_connection_t *exec_conn,
                                          const char *paramValues[MAX_PARAMS]);

/* === execute/finalize === */

/* Execute regular prepared/parametrized write and finalize statement write state. */
step_result_t step_write_execute_and_finalize(pg_stmt_t *pg_stmt,
                                              pg_connection_t *exec_conn,
                                              const char *paramValues[MAX_PARAMS],
                                              int *pg_conn_error_out);

/* Execute cached write statement text and create/update cached stmt bookkeeping. */
step_result_t step_cached_write_execute_and_finalize(pg_stmt_t **cached_io,
                                                     sqlite3_stmt *pStmt,
                                                     pg_connection_t *changes_conn,
                                                     pg_connection_t *exec_conn,
                                                     const char *orig_sql,
                                                     const char *exec_sql,
                                                     int *pg_conn_error_out);

/* === debug === */

/* Debug/trace hooks kept outside orchestration module. */
void step_write_log_debug_context(pg_stmt_t *pg_stmt,
                                  pg_connection_t *exec_conn,
                                  const char *paramValues[MAX_PARAMS]);
void step_log_step_exit_trace(pg_stmt_t *pg_stmt);

#endif
