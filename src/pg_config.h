/*
 * PostgreSQL Shim - Configuration Module
 * Configuration loading and SQL classification
 */

#ifndef PG_CONFIG_H
#define PG_CONFIG_H

#include "pg_types.h"

// Configuration loading
void pg_config_init(void);
pg_conn_config_t* pg_config_get(void);

// Retry delay schedule (PLEX_PG_RETRY_DELAYS env var)
// delays_out: array of at least PG_RETRY_MAX_DELAYS ints, filled with ms values
// count_out:  number of delays actually set
#define PG_RETRY_MAX_DELAYS 10
void pg_get_retry_delays(int *delays_out, int *count_out);

// SQL classification
int should_redirect(const char *filename);
int should_skip_sql(const char *sql);
int is_write_operation(const char *sql);
int is_read_operation(const char *sql);

#endif // PG_CONFIG_H
