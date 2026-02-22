/*
 * PostgreSQL Shim - Configuration Module
 * Thin C wrapper delegating all logic to the Rust pg_config FFI module.
 */

#include "pg_config.h"
#include "pg_logging.h"
#include <string.h>

extern int shim_passthrough_only;

// ============================================================================
// Rust FFI Declarations
// ============================================================================

extern int pg_config_should_redirect(const char *filename, int passthrough_only);
extern int pg_config_should_skip_sql(const char *sql);
extern int pg_config_is_write_operation(const char *sql);
extern int pg_config_is_read_operation(const char *sql);
extern void pg_config_get_retry_delays(int *delays_out, int *count_out);

typedef struct {
    char host[256];
    int port;
    char database[128];
    char user[128];
    char password[256];
    char schema[64];
} RustPgConnConfig;

extern int pg_config_load(RustPgConnConfig *config);

// ============================================================================
// Static State
// ============================================================================

static pg_conn_config_t pg_config;
static int config_loaded = 0;

// ============================================================================
// Configuration Loading
// ============================================================================

void pg_config_init(void) {
    if (config_loaded) return;

    RustPgConnConfig rust_cfg;
    pg_config_load(&rust_cfg);

    strncpy(pg_config.host,     rust_cfg.host,     sizeof(pg_config.host)     - 1);
    pg_config.host[sizeof(pg_config.host) - 1] = '\0';
    pg_config.port = rust_cfg.port;
    strncpy(pg_config.database, rust_cfg.database, sizeof(pg_config.database) - 1);
    pg_config.database[sizeof(pg_config.database) - 1] = '\0';
    strncpy(pg_config.user,     rust_cfg.user,     sizeof(pg_config.user)     - 1);
    pg_config.user[sizeof(pg_config.user) - 1] = '\0';
    strncpy(pg_config.password, rust_cfg.password, sizeof(pg_config.password) - 1);
    pg_config.password[sizeof(pg_config.password) - 1] = '\0';
    strncpy(pg_config.schema,   rust_cfg.schema,   sizeof(pg_config.schema)   - 1);
    pg_config.schema[sizeof(pg_config.schema) - 1] = '\0';

    config_loaded = 1;

    LOG_INFO("PostgreSQL config: %s@%s:%d/%s (schema: %s)",
             pg_config.user, pg_config.host, pg_config.port,
             pg_config.database, pg_config.schema);
}

pg_conn_config_t* pg_config_get(void) {
    if (!config_loaded) pg_config_init();
    return &pg_config;
}

// ============================================================================
// SQL Classification
// ============================================================================

int should_redirect(const char *filename) {
    return pg_config_should_redirect(filename, shim_passthrough_only);
}

int should_skip_sql(const char *sql) {
    return pg_config_should_skip_sql(sql);
}

int is_write_operation(const char *sql) {
    return pg_config_is_write_operation(sql);
}

int is_read_operation(const char *sql) {
    return pg_config_is_read_operation(sql);
}

// ============================================================================
// Retry Delay Configuration
// ============================================================================

void pg_get_retry_delays(int *delays_out, int *count_out) {
    pg_config_get_retry_delays(delays_out, count_out);
}
