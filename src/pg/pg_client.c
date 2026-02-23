/*
 * PostgreSQL Shim - Client/Connection Module (thin C shim)
 *
 * Pool management is delegated to Rust FFI (rust/plex-pg-core/src/pg_client.rs).
 * This file retains only:
 *   - libpq-dependent helpers (socket timeout, create_pool_connection, connect/close/ensure)
 *   - Connection registry (connections[] array + hash table for O(1) lookup by sqlite3*)
 *   - Prepared statement cache (operates on embedded stmt_cache_t in pg_connection_t)
 *   - Thin shim functions that forward to rust_pool_* FFI exports
 *   - C callback implementations that Rust calls via function pointers
 */

#include "pg_client.h"
#include "pg_config.h"
#include "pg_logging.h"
#include "db_interpose_rust.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <stdint.h>
#include <stdatomic.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <signal.h>
#include <unistd.h>
#include "shim_alloc.h"

// ============================================================================
// Socket timeout for PostgreSQL connections (prevents infinite poll() waits)
// ============================================================================

#define PG_SOCKET_TIMEOUT_SEC 60

// ============================================================================
// Static State (C-side registry, kept for handle lookup and non-pooled conns)
// ============================================================================

static pg_connection_t *connections[MAX_CONNECTIONS];
static pthread_mutex_t connections_mutex = PTHREAD_MUTEX_INITIALIZER;

static _Atomic int client_initialized = 0;
static pthread_once_t client_init_once = PTHREAD_ONCE_INIT;

// Forward declarations
static int is_library_db(const char *path);
static pg_connection_t* create_pool_connection(const char *db_path);

// ============================================================================
// libpq-dependent helpers (stay in C)
// ============================================================================

// Set socket timeouts on PostgreSQL connection to prevent infinite waits
static void pg_set_socket_timeout(PGconn *pg_conn) {
    if (!pg_conn) return;

    int sock = PQsocket(pg_conn);
    if (sock < 0) {
        LOG_ERROR("pg_set_socket_timeout: invalid socket");
        return;
    }

    struct timeval tv;
    tv.tv_sec = PG_SOCKET_TIMEOUT_SEC;
    tv.tv_usec = 0;

    if (setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) < 0) {
        LOG_ERROR("pg_set_socket_timeout: failed to set SO_RCVTIMEO");
    }
    if (setsockopt(sock, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv)) < 0) {
        LOG_ERROR("pg_set_socket_timeout: failed to set SO_SNDTIMEO");
    }

    LOG_DEBUG("Socket timeout set to %d seconds for socket %d", PG_SOCKET_TIMEOUT_SEC, sock);
}

// Create a new pool connection (allocate pg_connection_t, PQconnectdb, setup)
static pg_connection_t* create_pool_connection(const char *db_path) {
    pg_conn_config_t *cfg = pg_config_get();

    LOG_DEBUG("create_pool_connection: host='%s' port=%d db='%s' user='%s' schema='%s'",
              cfg->host, cfg->port, cfg->database, cfg->user, cfg->schema);

    // If config is empty (env vars not set), don't even try to connect
    if (cfg->host[0] == '\0' || cfg->port == 0) {
        LOG_ERROR("Pool connection skipped: config not loaded (host='%s' port=%d). "
                  "Check PLEX_PG_HOST/PLEX_PG_PORT env vars.",
                  cfg->host, cfg->port);
        return NULL;
    }

    pg_connection_t *conn = calloc(1, sizeof(pg_connection_t));
    if (!conn) {
        LOG_ERROR("Failed to allocate pg_connection_t for pool");
        return NULL;
    }

    pthread_mutex_init(&conn->mutex, NULL);
    strncpy(conn->db_path, db_path ? db_path : "", sizeof(conn->db_path) - 1);

    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s "
             "connect_timeout=5 keepalives=1 keepalives_idle=30 "
             "keepalives_interval=10 keepalives_count=3",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    conn->conn = PQconnectdb(conninfo);

    if (PQstatus(conn->conn) != CONNECTION_OK) {
        const char *err = conn->conn ? PQerrorMessage(conn->conn) : "NULL connection";
        LOG_ERROR("Pool connection failed: %s", err);
        if (conn->conn) {
            PQfinish(conn->conn);
        }
        conn->conn = NULL;
    } else {
        pg_set_socket_timeout(conn->conn);

        char schema_cmd[256];
        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
        PGresult *res = PQexec(conn->conn, schema_cmd);
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            const char *err = res ? PQresultErrorMessage(res) : "NULL result";
            LOG_ERROR("Failed to set search_path: %s", err);
        }
        if (res) PQclear(res);

        // Deallocate any leftover prepared statements from previous shim instance
        res = PQexec(conn->conn, "DEALLOCATE ALL");
        if (res) PQclear(res);

        res = PQexec(conn->conn, "SET statement_timeout = '60s'");
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            LOG_ERROR("Failed to set statement_timeout: %s", PQresultErrorMessage(res));
        }
        if (res) PQclear(res);

        conn->is_pg_active = 1;
    }

    return conn;
}

// Fast suffix check for library.db
static int is_library_db(const char *path) {
    return rust_is_library_db_path(path);
}

// ============================================================================
// C callback implementations for Rust to call via function pointers
// ============================================================================

// Create a new pool connection (allocate pg_connection_t, PQconnectdb, setup)
static void* cb_create_conn(const char *db_path) {
    return (void*)create_pool_connection(db_path);
}

// Destroy a pool connection (PQfinish + free)
static void cb_destroy_conn(void *conn_ptr) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (!conn) return;
    if (conn->conn) {
        PQfinish(conn->conn);
    }
    pthread_mutex_destroy(&conn->mutex);
    free(conn);
}

// Check if PQstatus == CONNECTION_OK
static int cb_check_conn_ok(void *conn_ptr) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (!conn || !conn->conn) return 0;
    return PQstatus(conn->conn) == CONNECTION_OK ? 1 : 0;
}

// PQreset + re-apply settings. Returns 1 on success.
static int cb_reset_conn(void *conn_ptr) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (!conn || !conn->conn) return 0;
    PQreset(conn->conn);
    if (PQstatus(conn->conn) != CONNECTION_OK) return 0;
    // Re-apply settings
    pg_set_socket_timeout(conn->conn);
    pg_conn_config_t *cfg = pg_config_get();
    char schema_cmd[256];
    snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
    PGresult *res = PQexec(conn->conn, schema_cmd);
    PQclear(res);
    res = PQexec(conn->conn, "SET statement_timeout = '60s'");
    PQclear(res);
    return 1;
}

// Full reconnect: PQfinish old, PQconnectdb new. Returns 1 on success.
static int cb_reconnect_slot(void *conn_ptr) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (!conn) return 0;
    // Lock mutex, PQfinish old
    pthread_mutex_lock(&conn->mutex);
    if (conn->conn) { PQfinish(conn->conn); conn->conn = NULL; }
    pthread_mutex_unlock(&conn->mutex);
    // Build conninfo, PQconnectdb
    pg_conn_config_t *cfg = pg_config_get();
    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s "
             "connect_timeout=5 keepalives=1 keepalives_idle=30 "
             "keepalives_interval=10 keepalives_count=3",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);
    PGconn *new_pg = PQconnectdb(conninfo);
    if (PQstatus(new_pg) == CONNECTION_OK) {
        pg_set_socket_timeout(new_pg);
        char schema_cmd[256];
        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
        PGresult *res = PQexec(new_pg, schema_cmd);
        PQclear(res);
        res = PQexec(new_pg, "SET statement_timeout = '60s'");
        PQclear(res);
        conn->conn = new_pg;
        conn->is_pg_active = 1;
        return 1;
    } else {
        LOG_ERROR("Pool: reconnect failed: %s", PQerrorMessage(new_pg));
        PQfinish(new_pg);
        conn->conn = NULL;
        conn->is_pg_active = 0;
        return 0;
    }
}

// Get PQtransactionStatus. Returns int matching PGTransactionStatusType.
static int cb_get_txn_status(void *conn_ptr) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (!conn || !conn->conn) return 0;  // PQTRANS_IDLE
    return (int)PQtransactionStatus(conn->conn);
}

// Execute simple SQL (e.g. "COMMIT", "ROLLBACK"). Returns 1 on success.
static int cb_exec_simple(void *conn_ptr, const char *sql) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (!conn || !conn->conn || !sql) return 0;
    PGresult *res = PQexec(conn->conn, sql);
    int ok = res && PQresultStatus(res) == PGRES_COMMAND_OK;
    if (res) PQclear(res);
    return ok ? 1 : 0;
}

// Check if streaming_active atomic flag is set
static int cb_is_streaming_active(void *conn_ptr) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (!conn) return 0;
    return atomic_load(&conn->streaming_active) ? 1 : 0;
}

// Check if is_pg_active
static int cb_is_pg_active(void *conn_ptr) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (!conn) return 0;
    return conn->is_pg_active ? 1 : 0;
}

// Set is_pg_active
static void cb_set_pg_active(void *conn_ptr, int active) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (conn) conn->is_pg_active = active;
}

// Check if a thread is still alive (pthread_kill(thread, 0))
static int cb_check_thread_alive(uint64_t thread_id) {
    pthread_t t;
    memcpy(&t, &thread_id, sizeof(t));
    return (pthread_kill(t, 0) == 0) ? 1 : 0;
}

// Clear stmt cache for a connection
static void cb_stmt_cache_clear(void *conn_ptr) {
    pg_stmt_cache_clear((pg_connection_t *)conn_ptr);
}

// Get db_path from a connection
static void cb_get_db_path(void *conn_ptr, char *buf, size_t len) {
    pg_connection_t *conn = (pg_connection_t *)conn_ptr;
    if (conn && len > 0) {
        strncpy(buf, conn->db_path, len - 1);
        buf[len - 1] = '\0';
    } else if (len > 0) {
        buf[0] = '\0';
    }
}

// Get current thread ID as uint64_t
static uint64_t cb_get_current_thread(void) {
    pthread_t t = pthread_self();
    uint64_t result = 0;
    memcpy(&result, &t, sizeof(t) < sizeof(result) ? sizeof(t) : sizeof(result));
    return result;
}

// Check if two thread IDs are equal
static int cb_threads_equal(uint64_t a, uint64_t b) {
    pthread_t ta, tb;
    memcpy(&ta, &a, sizeof(ta));
    memcpy(&tb, &b, sizeof(tb));
    return pthread_equal(ta, tb) ? 1 : 0;
}

// Sleep for ms milliseconds
static void cb_sleep_ms(int ms) {
    usleep(ms * 1000);
}

// Get retry delays from config
static void cb_get_retry_delays(int *delays, int *count) {
    pg_get_retry_delays(delays, count);
}

// Logging callbacks
static void cb_log_info(const char *msg) { LOG_INFO("%s", msg); }
static void cb_log_error(const char *msg) { LOG_ERROR("%s", msg); }
static void cb_log_debug(const char *msg) { LOG_DEBUG("%s", msg); }

// ============================================================================
// Initialization (thin shim -> Rust)
// ============================================================================

static void do_client_init(void) {
    memset(connections, 0, sizeof(connections));

    // Read pool size from environment (default: 50, max: 200)
    int pool_size = POOL_SIZE_DEFAULT;
    const char *pool_env = getenv("PLEX_PG_POOL_SIZE");
    if (pool_env) {
        int size = atoi(pool_env);
        if (size > 0 && size <= POOL_SIZE_MAX) {
            pool_size = size;
        } else if (size > POOL_SIZE_MAX) {
            pool_size = POOL_SIZE_MAX;
            LOG_INFO("PLEX_PG_POOL_SIZE=%d exceeds max, using %d", size, POOL_SIZE_MAX);
        }
    }

    // Read idle timeout from environment (default: 300s)
    int idle_timeout = 300;
    const char *idle_env = getenv("PLEX_PG_IDLE_TIMEOUT");
    if (idle_env) {
        int timeout = atoi(idle_env);
        if (timeout >= 10) {
            idle_timeout = timeout;
        }
    }

    // Register all C callbacks with Rust
    rust_pool_set_callbacks(
        cb_create_conn,
        cb_destroy_conn,
        cb_check_conn_ok,
        cb_reset_conn,
        cb_reconnect_slot,
        cb_get_txn_status,
        cb_exec_simple,
        cb_is_streaming_active,
        cb_is_pg_active,
        cb_set_pg_active,
        cb_check_thread_alive,
        cb_stmt_cache_clear,
        cb_get_db_path,
        cb_get_current_thread,
        cb_threads_equal,
        cb_sleep_ms,
        cb_get_retry_delays,
        cb_log_info,
        cb_log_error,
        cb_log_debug
    );

    // Initialize Rust pool
    rust_pool_init(pool_size, idle_timeout);

    client_initialized = 1;
    LOG_INFO("pg_client initialized (Rust pool): pool_size=%d, max=%d, idle_timeout=%ds",
             pool_size, POOL_SIZE_MAX, idle_timeout);
}

void pg_client_init(void) {
    pthread_once(&client_init_once, do_client_init);
}

void pg_client_cleanup(void) {
    // Clean up C-side connections array (non-pooled connections)
    pthread_mutex_lock(&connections_mutex);
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i]) {
            if (connections[i]->conn) {
                PQfinish(connections[i]->conn);
            }
            pthread_mutex_destroy(&connections[i]->mutex);
            free(connections[i]);
            connections[i] = NULL;
        }
    }
    pthread_mutex_unlock(&connections_mutex);

    // Delegate pool cleanup to Rust
    rust_pool_cleanup();

    client_initialized = 0;
}

// ============================================================================
// Connection Registry (C-side hash table + Rust registration)
// ============================================================================

void pg_register_connection(pg_connection_t *conn) {
    if (!conn) return;

    pthread_mutex_lock(&connections_mutex);

    // Add to array (for iteration/cleanup)
    int slot = -1;
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i] == NULL) {
            connections[i] = conn;
            slot = i;
            break;
        }
    }

    if (slot < 0) {
        pthread_mutex_unlock(&connections_mutex);
        LOG_ERROR("Connection registry full! MAX_CONNECTIONS=%d", MAX_CONNECTIONS);
        return;
    }

    LOG_DEBUG("Registered connection %p at slot %d", (void*)conn, slot);
    pthread_mutex_unlock(&connections_mutex);

    // Also register with Rust (for db-to-pool mapping)
    if (conn->shadow_db) {
        rust_register_connection(conn->shadow_db, conn);
    }
}

void pg_unregister_connection(pg_connection_t *conn) {
    if (!conn) return;

    pthread_mutex_lock(&connections_mutex);

    // Remove from array
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i] == conn) {
            connections[i] = NULL;
            break;
        }
    }

    LOG_DEBUG("Unregistered connection %p", (void*)conn);
    pthread_mutex_unlock(&connections_mutex);

    // Also unregister from Rust
    if (conn->shadow_db) {
        rust_unregister_connection(conn->shadow_db);
    }
}

// Find the handle connection (registered connection) for a sqlite3* handle
// Returns the connection registered in connections[] array, NOT a pool connection
pg_connection_t* pg_find_handle_connection(sqlite3 *db) {
    if (!db) return NULL;
    return (pg_connection_t *)rust_find_registered_connection(db);
}

// ============================================================================
// Connection Lookup (thin shim -> Rust for pool, C for handle registry)
// ============================================================================

pg_connection_t* pg_find_connection(sqlite3 *db) {
    if (!db) return NULL;

    // Check fork safety (Rust side)
    rust_pool_check_fork();
    pg_connection_t *handle_conn =
        (pg_connection_t *)rust_find_registered_connection(db);
    if (!handle_conn) return NULL;

    // Copy path to avoid using mutable shared storage directly.
    char path_copy[512];
    strncpy(path_copy, handle_conn->db_path, sizeof(path_copy) - 1);
    path_copy[sizeof(path_copy) - 1] = '\0';

    // For library.db, use Rust pool lookup.
    if (is_library_db(path_copy)) {
        const char *force_sqlite = getenv("PLEX_PG_FORCE_SQLITE_LIBRARY");
        if (force_sqlite && strcmp(force_sqlite, "0") != 0) {
            return NULL;
        }

        const char *disable_pool = getenv("PLEX_PG_DISABLE_POOL");
        if (disable_pool && strcmp(disable_pool, "0") != 0) {
            if (handle_conn && handle_conn->is_pg_active) {
                return handle_conn;
            }
            return NULL;
        }

        pg_connection_t *pool_conn =
            (pg_connection_t *)rust_pool_find_connection(db, path_copy);
        if (pool_conn && pool_conn->is_pg_active) {
            return pool_conn;
        }
        // Pool full — fall back to SQLite
        LOG_DEBUG("Pool full for library.db, falling back to SQLite");
        return NULL;
    }

    // Non-library.db: use registered handle connection directly.
    return handle_conn->is_pg_active ? handle_conn : NULL;
}

pg_connection_t* pg_find_any_library_connection(void) {
    pg_connection_t *result =
        (pg_connection_t *)rust_find_any_library_connection();
    if (result && result->is_pg_active) {
        return result;
    }

    // Fall back to any registered library connection in the C array
    pthread_mutex_lock(&connections_mutex);
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        if (connections[i] && connections[i]->is_pg_active &&
            strstr(connections[i]->db_path, "com.plexapp.plugins.library.db")) {
            pg_connection_t *conn = connections[i];
            pthread_mutex_unlock(&connections_mutex);
            return conn;
        }
    }
    pthread_mutex_unlock(&connections_mutex);
    return NULL;
}

// ============================================================================
// Thin shim functions forwarding to Rust
// ============================================================================

pg_connection_t* pg_get_thread_connection(const char *db_path) {
    return (pg_connection_t *)rust_pool_get_connection(db_path);
}

int pg_pool_validate_connection(pg_connection_t *conn) {
    return rust_pool_validate_connection(conn);
}

void pg_pool_touch_connection(pg_connection_t *conn) {
    rust_pool_touch_connection(conn);
}

int pg_pool_check_connection_health(pg_connection_t *conn) {
    return rust_pool_check_health(conn);
}

void pg_close_pool_for_db(sqlite3 *db) {
    if (!db) return;
    rust_pool_release_for_db(db);
}

sqlite3_int64 pg_get_global_metadata_id(void) {
    return (sqlite3_int64)rust_get_global_metadata_id();
}

void pg_set_global_metadata_id(sqlite3_int64 id) {
    rust_set_global_metadata_id((int64_t)id);
}

sqlite3_int64 pg_get_global_last_insert_rowid(void) {
    return (sqlite3_int64)rust_get_global_last_insert_rowid();
}

void pg_set_global_last_insert_rowid(sqlite3_int64 id) {
    rust_set_global_last_insert_rowid((int64_t)id);
}

void pg_pool_cleanup_after_fork(void) {
    // Clear C-side connection handles (don't free — parent owns).
    for (int i = 0; i < MAX_CONNECTIONS; i++) {
        connections[i] = NULL;
    }

    // Delegate pool state cleanup to Rust
    rust_pool_cleanup_after_fork();
}

// ============================================================================
// Connection Lifecycle (non-pooled connections, stays in C)
// ============================================================================

pg_connection_t* pg_connect(const char *db_path, sqlite3 *shadow_db) {
    pg_conn_config_t *cfg = pg_config_get();

    pg_connection_t *conn = calloc(1, sizeof(pg_connection_t));
    if (!conn) {
        LOG_ERROR("Failed to allocate pg_connection_t");
        return NULL;
    }

    pthread_mutex_init(&conn->mutex, NULL);
    conn->shadow_db = shadow_db;
    strncpy(conn->db_path, db_path ? db_path : "", sizeof(conn->db_path) - 1);

    // For library.db, DON'T create a real PostgreSQL connection here.
    // All queries will go through the connection pool via pg_find_connection().
    if (is_library_db(db_path)) {
        conn->conn = NULL;
        conn->is_pg_active = 1;
        LOG_INFO("PostgreSQL pool-only connection for: %s", db_path);
        return conn;
    }

    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s connect_timeout=5",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    conn->conn = PQconnectdb(conninfo);

    if (PQstatus(conn->conn) != CONNECTION_OK) {
        const char *err = conn->conn ? PQerrorMessage(conn->conn) : "NULL connection";
        LOG_ERROR("PostgreSQL connection failed: %s", err);
        if (conn->conn) {
            PQfinish(conn->conn);
        }
        conn->conn = NULL;
    } else {
        LOG_INFO("PostgreSQL connected for: %s", db_path);

        pg_set_socket_timeout(conn->conn);

        char schema_cmd[256];
        snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
        PGresult *res = PQexec(conn->conn, schema_cmd);
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            const char *err = res ? PQresultErrorMessage(res) : "NULL result";
            LOG_ERROR("Failed to set search_path: %s", err);
        }
        if (res) PQclear(res);

        res = PQexec(conn->conn, "SET statement_timeout = '60s'");
        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            LOG_ERROR("Failed to set statement_timeout: %s", PQresultErrorMessage(res));
        }
        if (res) PQclear(res);

        conn->is_pg_active = 1;
    }

    return conn;
}

int pg_ensure_connection(pg_connection_t *conn) {
    if (!conn) return 0;

    pthread_mutex_lock(&conn->mutex);

    if (conn->conn && PQstatus(conn->conn) == CONNECTION_OK) {
        PGresult *res = PQexec(conn->conn, "SELECT 1");
        if (res && PQresultStatus(res) == PGRES_TUPLES_OK) {
            PQclear(res);
            pthread_mutex_unlock(&conn->mutex);
            return 1;
        }
        if (res) PQclear(res);
        LOG_INFO("Connection health check failed, will reconnect");
    }

    if (conn->conn) {
        PQfinish(conn->conn);
        conn->conn = NULL;
    }

    pg_conn_config_t *cfg = pg_config_get();
    char conninfo[1024];
    snprintf(conninfo, sizeof(conninfo),
             "host=%s port=%d dbname=%s user=%s password=%s connect_timeout=5",
             cfg->host, cfg->port, cfg->database, cfg->user, cfg->password);

    conn->conn = PQconnectdb(conninfo);

    if (PQstatus(conn->conn) != CONNECTION_OK) {
        const char *err = conn->conn ? PQerrorMessage(conn->conn) : "NULL connection";
        LOG_ERROR("PostgreSQL reconnection failed: %s", err);
        if (conn->conn) {
            PQfinish(conn->conn);
        }
        conn->conn = NULL;
        conn->is_pg_active = 0;
        pthread_mutex_unlock(&conn->mutex);
        return 0;
    }

    LOG_INFO("PostgreSQL reconnected successfully");

    pg_set_socket_timeout(conn->conn);

    char schema_cmd[256];
    snprintf(schema_cmd, sizeof(schema_cmd), "SET search_path TO %s, public", cfg->schema);
    PGresult *res = PQexec(conn->conn, schema_cmd);
    if (PQresultStatus(res) != PGRES_COMMAND_OK) {
        const char *err = res ? PQresultErrorMessage(res) : "NULL result";
        LOG_ERROR("Failed to set search_path on reconnect: %s", err);
    }
    if (res) PQclear(res);

    res = PQexec(conn->conn, "SET statement_timeout = '60s'");
    if (PQresultStatus(res) != PGRES_COMMAND_OK) {
        LOG_ERROR("Failed to set statement_timeout on reconnect: %s", PQresultErrorMessage(res));
    }
    if (res) PQclear(res);

    conn->is_pg_active = 1;
    pthread_mutex_unlock(&conn->mutex);
    return 1;
}

void pg_close(pg_connection_t *conn) {
    if (!conn) return;

    pg_stmt_cache_clear(conn);

    pthread_mutex_lock(&conn->mutex);
    if (conn->conn) {
        PQfinish(conn->conn);
        conn->conn = NULL;
    }
    pthread_mutex_unlock(&conn->mutex);

    pthread_mutex_destroy(&conn->mutex);
    free(conn);
}

// ============================================================================
// Prepared Statement Cache Management (operates on embedded stmt_cache_t)
// ============================================================================

// FNV-1a hash - delegated to Rust
uint64_t pg_hash_sql(const char *sql) {
    return rust_hash_sql(sql);
}

// Lookup statement in cache by hash (O(1) average with linear probing)
int pg_stmt_cache_lookup(pg_connection_t *conn, uint64_t sql_hash, const char **stmt_name) {
    if (!conn || !stmt_name || sql_hash == 0) return 0;

    stmt_cache_t *cache = &conn->stmt_cache;
    int start_idx = (int)(sql_hash & STMT_CACHE_MASK);

    for (int probe = 0; probe < STMT_CACHE_SIZE; probe++) {
        int idx = (start_idx + probe) & STMT_CACHE_MASK;
        prepared_stmt_cache_entry_t *entry = &cache->entries[idx];

        if (entry->sql_hash == 0) {
            return 0;
        }

        if (entry->sql_hash == sql_hash && entry->prepared) {
            entry->last_used = time(NULL);
            *stmt_name = entry->stmt_name;
            return 1;
        }
    }

    return 0;
}

// Add statement to cache (O(1) average with linear probing)
int pg_stmt_cache_add(pg_connection_t *conn, uint64_t sql_hash, const char *stmt_name, int param_count) {
    if (!conn || !stmt_name || sql_hash == 0) return -1;

    stmt_cache_t *cache = &conn->stmt_cache;
    int start_idx = (int)(sql_hash & STMT_CACHE_MASK);
    int oldest_idx = -1;
    time_t oldest_time = 0;

    for (int probe = 0; probe < STMT_CACHE_SIZE; probe++) {
        int idx = (start_idx + probe) & STMT_CACHE_MASK;
        prepared_stmt_cache_entry_t *entry = &cache->entries[idx];

        if (oldest_idx == -1 || (entry->sql_hash != 0 && entry->last_used < oldest_time)) {
            oldest_idx = idx;
            oldest_time = entry->last_used;
        }

        if (entry->sql_hash == sql_hash) {
            entry->prepared = 1;
            entry->param_count = param_count;
            entry->last_used = time(NULL);
            strncpy(entry->stmt_name, stmt_name, sizeof(entry->stmt_name) - 1);
            entry->stmt_name[sizeof(entry->stmt_name) - 1] = '\0';
            LOG_DEBUG("Updated prepared statement in cache: %s (hash=%llx, idx=%d)",
                      stmt_name, (unsigned long long)sql_hash, idx);
            return idx;
        }

        if (entry->sql_hash == 0) {
            entry->sql_hash = sql_hash;
            entry->param_count = param_count;
            entry->prepared = 1;
            entry->last_used = time(NULL);
            strncpy(entry->stmt_name, stmt_name, sizeof(entry->stmt_name) - 1);
            entry->stmt_name[sizeof(entry->stmt_name) - 1] = '\0';
            cache->count++;
            LOG_DEBUG("Added prepared statement to cache: %s (hash=%llx, idx=%d)",
                      stmt_name, (unsigned long long)sql_hash, idx);
            return idx;
        }
    }

    // Cache full - evict oldest entry
    if (oldest_idx >= 0) {
        prepared_stmt_cache_entry_t *entry = &cache->entries[oldest_idx];

        if (entry->prepared && conn->conn) {
            char dealloc[64];
            snprintf(dealloc, sizeof(dealloc), "DEALLOCATE %s", entry->stmt_name);
            PGresult *res = PQexec(conn->conn, dealloc);
            if (res) PQclear(res);
            LOG_DEBUG("Evicted prepared statement from cache: %s", entry->stmt_name);
        }

        entry->sql_hash = sql_hash;
        entry->param_count = param_count;
        entry->prepared = 1;
        entry->last_used = time(NULL);
        strncpy(entry->stmt_name, stmt_name, sizeof(entry->stmt_name) - 1);
        entry->stmt_name[sizeof(entry->stmt_name) - 1] = '\0';
        LOG_DEBUG("Added prepared statement (evicted): %s (hash=%llx, idx=%d)",
                  stmt_name, (unsigned long long)sql_hash, oldest_idx);
        return oldest_idx;
    }

    return -1;
}

// Check SQLSTATE 26000 (invalid_sql_statement_name)
int pg_is_stale_prepared_stmt(PGresult *res) {
    if (!res) return 0;
    const char *sqlstate = PQresultErrorField(res, PG_DIAG_SQLSTATE);
    return rust_is_stale_sqlstate(sqlstate);
}

// Check SQLSTATE 42P05 (duplicate_prepared_statement)
int pg_is_duplicate_prepared_stmt(PGresult *res) {
    if (!res) return 0;
    const char *sqlstate = PQresultErrorField(res, PG_DIAG_SQLSTATE);
    return rust_is_duplicate_sqlstate(sqlstate);
}

// Clear local prepared statement cache without sending DEALLOCATE to server
void pg_stmt_cache_clear_local(pg_connection_t *conn) {
    if (!conn) return;
    memset(&conn->stmt_cache, 0, sizeof(stmt_cache_t));
    LOG_INFO("Cleared prepared statement cache (local only) for connection %p", (void*)conn);
}

// Clear all cached statements for a connection (called on disconnect/reset)
void pg_stmt_cache_clear(pg_connection_t *conn) {
    if (!conn) return;

    stmt_cache_t *cache = &conn->stmt_cache;

    if (conn->conn) {
        for (int i = 0; i < STMT_CACHE_SIZE; i++) {
            if (cache->entries[i].sql_hash != 0 && cache->entries[i].prepared) {
                char dealloc[64];
                snprintf(dealloc, sizeof(dealloc), "DEALLOCATE %s", cache->entries[i].stmt_name);
                PGresult *res = PQexec(conn->conn, dealloc);
                if (res) PQclear(res);
            }
        }
    }

    memset(cache, 0, sizeof(stmt_cache_t));
    LOG_DEBUG("Cleared prepared statement cache for connection %p", (void*)conn);
}
