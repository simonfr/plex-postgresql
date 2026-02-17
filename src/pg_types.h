/*
 * PostgreSQL Shim - Type Definitions
 * Central type definitions shared across all modules
 */

#ifndef PG_TYPES_H
#define PG_TYPES_H

#include <stdint.h>
#include <sqlite3.h>
#include <libpq-fe.h>
#include <pthread.h>
#include <stdatomic.h>

// ============================================================================
// Constants
// ============================================================================

#define MAX_CONNECTIONS 512
#define MAX_PARAMS 256
#define MAX_STATEMENTS 1024
#define MAX_CACHED_STMTS_PER_THREAD 64
#define PG_VALUE_MAGIC 0x50475641  // "PGVA" - identifies our fake sqlite3_value

// Log file path
#define LOG_FILE "/tmp/plex_redirect_pg.log"
#define FALLBACK_LOG_FILE "/tmp/plex_pg_fallbacks.log"

// Environment variables
#define ENV_PG_HOST     "PLEX_PG_HOST"
#define ENV_PG_PORT     "PLEX_PG_PORT"
#define ENV_PG_DATABASE "PLEX_PG_DATABASE"
#define ENV_PG_USER     "PLEX_PG_USER"
#define ENV_PG_PASSWORD "PLEX_PG_PASSWORD"
#define ENV_PG_SCHEMA   "PLEX_PG_SCHEMA"
#define ENV_PG_LOG_LEVEL    "PLEX_PG_LOG_LEVEL"
#define ENV_PG_LOG_FILE     "PLEX_PG_LOG_FILE"
#define ENV_PG_LOG_MAX_SIZE "PLEX_PG_LOG_MAX_SIZE"
#define ENV_PG_EXCEPTION_LOG "PLEX_PG_EXCEPTION_LOG"

// PostgreSQL-only mode flag
#define PG_READ_ENABLED 1

// Connection pool size (max slots, actual limit via PLEX_PG_POOL_SIZE env var)
// Increased for Kometa bulk operations which can exceed 100 concurrent requests
#define POOL_SIZE_MAX 200
#define POOL_SIZE_DEFAULT 150

// Prepared statement cache size per connection (must be power of 2 for hash table)
#define STMT_CACHE_SIZE 512
#define STMT_CACHE_MASK (STMT_CACHE_SIZE - 1)

// ============================================================================
// Prepared Statement Cache (per-connection, hash table with linear probing)
// ============================================================================

typedef struct {
    uint64_t sql_hash;           // FNV-1a hash of SQL string (0 = empty slot)
    char stmt_name[32];          // "ps_<hash>" - PostgreSQL statement name
    int param_count;             // Number of parameters
    int prepared;                // 1 = prepared on this connection
    time_t last_used;            // For LRU eviction
} prepared_stmt_cache_entry_t;

typedef struct {
    prepared_stmt_cache_entry_t entries[STMT_CACHE_SIZE];
    int count;                   // Number of entries (for stats)
} stmt_cache_t;

// ============================================================================
// Connection Pool State Machine (thread-safe with atomic CAS)
// ============================================================================

typedef enum {
    SLOT_FREE = 0,        // Available for any thread
    SLOT_RESERVED,        // Thread claimed, creating connection
    SLOT_READY,           // Connection active and usable
    SLOT_RECONNECTING,    // Thread is reconnecting
    SLOT_ERROR            // Connection failed
} pool_slot_state_t;

// Forward declaration for pool_slot_t (full definition in pg_client.c)
struct pg_connection;

// ============================================================================
// Configuration Structure
// ============================================================================

typedef struct {
    char host[256];
    int port;
    char database[128];
    char user[128];
    char password[256];
    char schema[64];
} pg_conn_config_t;

// ============================================================================
// Connection Structure
// ============================================================================

typedef struct pg_connection {
    PGconn *conn;
    sqlite3 *shadow_db;              // Real SQLite handle for fallback/mapping
    char db_path[1024];
    int is_pg_active;                // Whether we're using PostgreSQL
    int in_transaction;
    pthread_mutex_t mutex;
    int last_changes;                // Track rows affected by last write
    sqlite3_int64 last_insert_rowid; // Track last inserted row ID
    sqlite3_int64 last_generator_metadata_id;  // Track metadata_item_id from generator URI
    char last_error[1024];           // Track last PostgreSQL error message
    int last_error_code;             // Track last SQLite-style error code

    // v0.9.29: Streaming mode lock — when 1, this connection is exclusively owned
    // by a streaming query (PQsetSingleRowMode). Other queries on the same thread
    // must use a different connection from the pool.
    volatile int streaming_active;

    // Prepared statement cache for this connection
    stmt_cache_t stmt_cache;
} pg_connection_t;

// ============================================================================
// Statement Structure
// ============================================================================

// ============================================================================
// Query Result Cache Types (for OnDeck optimization)
// ============================================================================

// Cached row data
typedef struct {
    char **values;          // Array of string values (NULL for NULL values)
    int *lengths;           // Length of each value
    int *is_null;           // 1 if value is NULL
} cached_row_t;

// Cached query result
typedef struct cached_result {
    uint64_t cache_key;     // Hash of SQL + params
    uint64_t created_ms;    // Timestamp when cached
    atomic_int ref_count;   // Reference count - don't free while > 0
    int num_rows;
    int num_cols;
    Oid *col_types;         // PostgreSQL type OIDs per column
    char **col_names;       // Column names
    cached_row_t *rows;     // Array of cached rows
    int hit_count;          // Number of cache hits (for stats)
} cached_result_t;

typedef struct pg_stmt {
    pthread_mutex_t mutex;           // Protect against concurrent access from multiple threads
    atomic_int ref_count;            // CRITICAL FIX: Reference count to prevent double-free
    pg_connection_t *conn;
    sqlite3_stmt *shadow_stmt;       // Real SQLite statement handle (for mapping)
    char *sql;                       // Original SQL
    char *pg_sql;                    // Translated PostgreSQL SQL
    PGresult *result;
    cached_result_t *cached_result;  // Pointer to cached result (when using query cache)

    // Prepared statement support
    uint64_t sql_hash;               // FNV-1a hash of pg_sql for cache lookup
    char stmt_name[32];              // "ps_<hash>" - PostgreSQL statement name
    int use_prepared;                // 1 = use prepared statements for this query
    int current_row;
    int num_rows;
    int num_cols;
    int is_pg;                       // 0=skip, 1=write, 2=read, 3=no-op
    int is_cached;                   // 1 if from TLS (cached stmt)
    int is_count_query;              // 1 if query contains "parents.parent_id,count(*)" (cached at prepare time)
    int needs_requery;               // 1 if reset() was called
    int write_executed;              // 1 if write has been executed (prevents duplicate execution)
    int read_done;                   // 1 if read has returned SQLITE_DONE (prevents re-execution)
    int metadata_only_result;        // v0.8.9: 1 if result is from pre-step metadata call (needs re-exec on bind)
    atomic_int in_step;              // v0.9.0: 1 if step operation is in progress (prevents concurrent bind)
    pthread_t executing_thread;      // Thread currently executing this statement (for debug)
    pg_connection_t *result_conn;    // Connection that the current result belongs to

    // Column names from PQdescribePrepared (when no result/cached_result yet)
    // Used by dummy prepare path: column metadata is available before step()
    char **col_names;                // Array of column name strings (NULL if not set)
    int num_col_names;               // Number of entries in col_names

    // Single-row streaming mode (v0.9.28)
    // Instead of fetching entire PGresult eagerly, use PQsetSingleRowMode
    // to fetch one row at a time — matching SQLite's step() memory model.
    int streaming_mode;              // 1 = single-row streaming active on this stmt
    pg_connection_t *streaming_conn; // Connection claimed for streaming (must drain before reuse)

    char *param_values[MAX_PARAMS];
    int param_lengths[MAX_PARAMS];
    int param_formats[MAX_PARAMS];   // 0 = text, 1 = binary
    int param_count;
    char **param_names;              // Named parameter names (for mapping :name to $N)
    char param_buffers[MAX_PARAMS][32]; // Pre-allocated buffers for int/double (avoid malloc)

    // Decoded BYTEA blob cache (per-row, freed on step/reset)
    void *decoded_blobs[MAX_PARAMS]; // Decoded binary data per column
    int decoded_blob_lens[MAX_PARAMS]; // Length of decoded data per column
    int decoded_blob_row;            // Row for which blobs are cached (-1 = none)

    // Cached text/blob values to ensure pointer validity per SQLite contract
    // These remain valid until step()/reset()/finalize()
    char *cached_text[MAX_PARAMS];   // Cached strdup'd text per column
    void *cached_blob[MAX_PARAMS];   // Cached blob data per column
    int cached_blob_len[MAX_PARAMS]; // Length of cached blob per column
    int cached_row;                  // Row for which values are cached (-1 = none)

    // Resolved table names for each column (for decltype lookup of bare columns)
    // Populated at query execution time using PQftable/PQftablecol
    char *col_table_names[MAX_PARAMS]; // Source table name for each column (NULL if unknown)
    int col_tables_resolved;           // 1 if table names have been resolved
} pg_stmt_t;

// ============================================================================
// Thread-Local Storage Structures
// ============================================================================

typedef struct {
    sqlite3_stmt *sqlite_stmt;
    pg_stmt_t *pg_stmt;
} cached_stmt_entry_t;

typedef struct {
    cached_stmt_entry_t entries[MAX_CACHED_STMTS_PER_THREAD];
    int count;
} thread_cached_stmts_t;

// ============================================================================
// Fake sqlite3_value for PostgreSQL columns
// ============================================================================

typedef struct pg_value {
    unsigned int magic;              // PG_VALUE_MAGIC to identify our values
    pg_stmt_t *stmt;                 // Parent statement
    int col_idx;                     // Column index
    int type;                        // SQLite type (SQLITE_INTEGER, etc)
} pg_value_t;

// ============================================================================
// DYLD Interpose Macro (macOS only)
// ============================================================================

#ifdef __APPLE__
#define DYLD_INTERPOSE(_replacement, _original) \
    __attribute__((used)) static struct { \
        const void* replacement; \
        const void* original; \
    } _interpose_##_original __attribute__((section("__DATA,__interpose"))) = { \
        (const void*)(unsigned long)&_replacement, \
        (const void*)(unsigned long)&_original \
    };
#endif

#endif // PG_TYPES_H
