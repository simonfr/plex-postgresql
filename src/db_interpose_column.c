/*
 * Plex PostgreSQL Interposing Shim - Column & Value Access
 *
 * Handles sqlite3_column_* and sqlite3_value_* function interposition.
 * These functions read data from PostgreSQL result sets.
 */

#include "db_interpose.h"
#include "pg_query_cache.h"
#include <stdatomic.h>
#include <sys/time.h>

// CRITICAL FIX v0.9.3: Thread-local flag to prevent recursion in resolve_column_tables
// When resolve_column_tables calls PQexec(), it can trigger Plex's SQLite hooks
// which call back into our shim → infinite recursion → stack overflow
// Declared in db_interpose.h, defined in db_interpose_common.c

// ============================================================================
// SQLite Declared Type Lookup Cache
// ============================================================================
// This cache stores original SQLite declared types from the plex.sqlite_column_types
// metadata table. SOCI ORM uses column_decltype for type validation, so we need
// to return the exact original SQLite types (e.g., "boolean", "dt_integer(8)")
// instead of PostgreSQL-derived types (e.g., "INTEGER", "TEXT").

#define DECLTYPE_CACHE_SIZE 1024
#define DECLTYPE_MAX_KEY_LEN 128
#define DECLTYPE_MAX_TYPE_LEN 64

typedef struct {
    char key[DECLTYPE_MAX_KEY_LEN];      // "table_column" key
    char decltype_val[DECLTYPE_MAX_TYPE_LEN];  // Original SQLite declared type
    int valid;                            // 1 = valid entry, 0 = empty/invalid
} decltype_cache_entry_t;

static decltype_cache_entry_t decltype_cache[DECLTYPE_CACHE_SIZE];
static pthread_mutex_t decltype_cache_mutex = PTHREAD_MUTEX_INITIALIZER;
static int decltype_cache_initialized = 0;
static int decltype_cache_loaded = 0;  // Have we loaded from DB?

// Hash function for cache lookup
static unsigned int decltype_hash(const char *str) {
    unsigned int hash = 5381;
    int c;
    while ((c = *str++)) {
        hash = ((hash << 5) + hash) + (unsigned char)c;
    }
    return hash;
}

// Preload all SQLite declared types from metadata table into cache
// Called once on first decltype request
static void preload_decltype_cache(pg_connection_t *pg_conn) {
    if (decltype_cache_loaded || !pg_conn || !pg_conn->conn) {
        return;
    }

    pthread_mutex_lock(&decltype_cache_mutex);
    if (decltype_cache_loaded) {
        pthread_mutex_unlock(&decltype_cache_mutex);
        return;
    }

    // Initialize cache
    if (!decltype_cache_initialized) {
        memset(decltype_cache, 0, sizeof(decltype_cache));
        decltype_cache_initialized = 1;
    }

    LOG_INFO("DECLTYPE_CACHE: Preloading SQLite declared types from metadata table...");

    // Query all types from metadata table
    pthread_mutex_lock(&pg_conn->mutex);
    PGresult *res = PQexec(pg_conn->conn,
        "SELECT table_name, column_name, declared_type FROM plex.sqlite_column_types");
    pthread_mutex_unlock(&pg_conn->mutex);

    if (!res || PQresultStatus(res) != PGRES_TUPLES_OK) {
        LOG_ERROR("DECLTYPE_CACHE: Failed to load metadata: %s",
                  res ? PQerrorMessage(pg_conn->conn) : "NULL result");
        if (res) PQclear(res);
        decltype_cache_loaded = 1;  // Mark as loaded (even if failed) to avoid retrying
        pthread_mutex_unlock(&decltype_cache_mutex);
        return;
    }

    int num_rows = PQntuples(res);
    int loaded = 0;
    int collisions = 0;

    for (int i = 0; i < num_rows; i++) {
        const char *table = PQgetvalue(res, i, 0);
        const char *column = PQgetvalue(res, i, 1);
        const char *decltype_str = PQgetvalue(res, i, 2);

        if (!table || !column || !decltype_str) continue;

        // Create cache key: "table_column"
        char key[DECLTYPE_MAX_KEY_LEN];
        snprintf(key, sizeof(key), "%s_%s", table, column);

        // Compute hash and find slot
        unsigned int hash = decltype_hash(key);
        int start_idx = hash % DECLTYPE_CACHE_SIZE;

        // Linear probing for collision resolution
        int found_slot = 0;
        for (int probe = 0; probe < 8; probe++) {
            int idx = (start_idx + probe) % DECLTYPE_CACHE_SIZE;
            if (!decltype_cache[idx].valid) {
                // Empty slot - use it
                strncpy(decltype_cache[idx].key, key, DECLTYPE_MAX_KEY_LEN - 1);
                decltype_cache[idx].key[DECLTYPE_MAX_KEY_LEN - 1] = '\0';
                strncpy(decltype_cache[idx].decltype_val, decltype_str, DECLTYPE_MAX_TYPE_LEN - 1);
                decltype_cache[idx].decltype_val[DECLTYPE_MAX_TYPE_LEN - 1] = '\0';
                decltype_cache[idx].valid = 1;
                loaded++;
                found_slot = 1;
                break;
            }
        }
        if (!found_slot) {
            collisions++;
        }
    }

    PQclear(res);
    decltype_cache_loaded = 1;
    pthread_mutex_unlock(&decltype_cache_mutex);

    LOG_INFO("DECLTYPE_CACHE: Loaded %d types (%d collisions/overflows)", loaded, collisions);
}

// Normalize Plex custom type annotations to standard SQLite types
// Plex uses "DT_INTEGER(8)" for BIGINT, "BOOLEAN" for booleans, etc.
// SQLite schema uses "INTEGER(8)" for bigint columns, VARCHAR(n) for strings,
// TIMESTAMP for timestamps, FLOAT for floats, etc.
// SOCI only understands: INTEGER, REAL, TEXT, BLOB
// Returns pointer to static string (do not free)
static const char* normalize_sqlite_decltype(const char *plex_type) {
    // BUG FIX 1: Never return NULL - SOCI defaults to "char" when NULL
    // Always return a valid type to prevent type mismatch errors
    if (!plex_type || !plex_type[0]) {
        LOG_DEBUG("NORMALIZE_TYPE: NULL/empty input, returning TEXT");
        return "TEXT";
    }

    // Check for Plex's DT_INTEGER(n) format
    // TEST 5: Keep DT_INTEGER(8) as-is, matching native SQLite
    if (strncasecmp(plex_type, "DT_INTEGER", 10) == 0) {
        // Check if it's DT_INTEGER(8) - keep it exactly as native SQLite stores it
        if ((plex_type[10] == '(' && plex_type[11] == '8' && plex_type[12] == ')') ||
            (plex_type[10] == '(' && plex_type[11] == '8' && plex_type[12] == ')')) {
            LOG_DEBUG("NORMALIZE_TYPE: 'DT_INTEGER(8)' -> 'dt_integer(8)' (match native SQLite)");
            return "dt_integer(8)";
        }
        // Otherwise treat as 32-bit INTEGER
        return "INTEGER";
    }

    // BUG FIX 2: Add boundary checks for prefix matches to prevent false matches
    // (e.g., "INTEGER_FIELD" should not match "INTEGER")
    
    // Check for INTEGER(n) format - e.g., INTEGER, INTEGER(8), integer, etc.
    // TEST 5: Return dt_integer(8) to match native SQLite
    if (strncasecmp(plex_type, "INTEGER", 7) == 0 && 
        (plex_type[7] == '\0' || plex_type[7] == '(' || isspace(plex_type[7]))) {
        // Check if it's INTEGER(8) - return native SQLite format
        if (plex_type[7] == '(' && plex_type[8] == '8' && plex_type[9] == ')') {
            LOG_DEBUG("NORMALIZE_TYPE: 'INTEGER(8)' -> 'dt_integer(8)' (match native SQLite)");
            return "dt_integer(8)";
        }
        // Otherwise treat as 32-bit INTEGER
        return "INTEGER";
    }

    // BUG FIX 3: Add BIGINT support for 64-bit integers
    // TEST 5: Convert BIGINT to dt_integer(8) to match native SQLite
    if (strncasecmp(plex_type, "BIGINT", 6) == 0 && 
        (plex_type[6] == '\0' || plex_type[6] == '(' || isspace(plex_type[6]))) {
        LOG_DEBUG("NORMALIZE_TYPE: 'BIGINT(...)' -> 'dt_integer(8)' (match native SQLite)");
        return "dt_integer(8)";
    }

    // INT8 returns dt_integer(8) (PostgreSQL alias)
    if (strcasecmp(plex_type, "INT8") == 0) {
        LOG_DEBUG("NORMALIZE_TYPE: 'INT8' -> 'dt_integer(8)' (match native SQLite)");
        return "dt_integer(8)";
    }
    
    // INT64 returns dt_integer(8)
    if (strcasecmp(plex_type, "INT64") == 0) {
        LOG_DEBUG("NORMALIZE_TYPE: 'INT64' -> 'dt_integer(8)' (match native SQLite)");
        return "dt_integer(8)";
    }
    
    // LONG returns dt_integer(8)
    if (strcasecmp(plex_type, "LONG") == 0) {
        LOG_DEBUG("NORMALIZE_TYPE: 'LONG' -> 'dt_integer(8)' (match native SQLite)");
        return "dt_integer(8)";
    }
    
    // dt_integer(8) stays as-is
    if (strcasecmp(plex_type, "dt_integer(8)") == 0) {
        return "dt_integer(8)";
    }

    // Normalize boolean to INTEGER (SQLite doesn't have native boolean)
    if (strcasecmp(plex_type, "boolean") == 0) {
        return "INTEGER";
    }

    // TIMESTAMP is stored as INTEGER in SQLite (unix timestamp)
    if (strcasecmp(plex_type, "TIMESTAMP") == 0) {
        return "INTEGER";
    }

    // FLOAT/DOUBLE types map to REAL
    if (strcasecmp(plex_type, "FLOAT") == 0) return "REAL";
    if (strcasecmp(plex_type, "DOUBLE") == 0) return "REAL";

    // VARCHAR(n) and STRING types map to TEXT - with boundary check
    if (strncasecmp(plex_type, "VARCHAR", 7) == 0 && 
        (plex_type[7] == '\0' || plex_type[7] == '(' || isspace(plex_type[7]))) {
        return "TEXT";
    }
    if (strcasecmp(plex_type, "STRING") == 0) return "TEXT";
    if (strcasecmp(plex_type, "CHAR") == 0) return "TEXT";

    // Standard SQLite types - exact match (case insensitive)
    if (strcasecmp(plex_type, "REAL") == 0) return "REAL";
    if (strcasecmp(plex_type, "TEXT") == 0) return "TEXT";
    if (strcasecmp(plex_type, "BLOB") == 0) return "BLOB";
    if (strcasecmp(plex_type, "NUMERIC") == 0) return "NUMERIC";

    // BUG FIX 4: Always return TEXT as safe default, NEVER return NULL
    LOG_DEBUG("NORMALIZE_TYPE: unknown type '%s', defaulting to TEXT", plex_type);
    return "TEXT";
}

// Direct cache lookup by exact table_column key (no parsing)
// Used when we already know the table name from PQftable()
// Returns normalized type or NULL if not found
static const char* lookup_decltype_direct(pg_connection_t *pg_conn, const char *cache_key) {
    if (!cache_key || !cache_key[0]) {
        return NULL;
    }

    // Ensure cache is loaded
    if (!decltype_cache_loaded && pg_conn) {
        preload_decltype_cache(pg_conn);
    }

    // Look up in cache
    unsigned int hash = decltype_hash(cache_key);
    int start_idx = hash % DECLTYPE_CACHE_SIZE;

    for (int probe = 0; probe < 8; probe++) {
        int idx = (start_idx + probe) % DECLTYPE_CACHE_SIZE;
        if (!decltype_cache[idx].valid) {
            break;  // Empty slot - not found
        }
        if (strcmp(decltype_cache[idx].key, cache_key) == 0) {
            const char *raw_type = decltype_cache[idx].decltype_val;
            const char *normalized = normalize_sqlite_decltype(raw_type);
            LOG_DEBUG("DECLTYPE_DIRECT: found '%s' -> '%s' (normalized to '%s')",
                     cache_key, raw_type, normalized);
            return normalized;
        }
    }

    LOG_DEBUG("DECLTYPE_DIRECT: '%s' not in cache", cache_key);
    return NULL;
}

// Look up original SQLite declared type from cache
// col_alias is like "devices_id" or "accounts_auto_select_subtitle"
// Returns static string (do not free), or NULL if not found
static const char* lookup_sqlite_decltype(pg_connection_t *pg_conn, const char *col_alias) {
    if (!col_alias || !col_alias[0]) {
        return NULL;
    }

    // Ensure cache is loaded
    if (!decltype_cache_loaded && pg_conn) {
        preload_decltype_cache(pg_conn);
    }

    // Parse alias: find first underscore to split table_column
    // Format is "table_column" - first part before underscore is table name
    const char *underscore = strchr(col_alias, '_');
    if (!underscore || underscore == col_alias) {
        LOG_DEBUG("DECLTYPE_LOOKUP: no underscore in '%s', cannot parse", col_alias);
        return NULL;
    }

    // Extract table name (everything before first underscore)
    size_t table_len = underscore - col_alias;
    if (table_len >= 64) table_len = 63;
    char table_name[64];
    memcpy(table_name, col_alias, table_len);
    table_name[table_len] = '\0';

    // Extract column name (everything after first underscore)
    const char *column_name = underscore + 1;
    if (!column_name[0]) {
        LOG_DEBUG("DECLTYPE_LOOKUP: empty column name in '%s'", col_alias);
        return NULL;
    }

    // Create cache key
    char cache_key[DECLTYPE_MAX_KEY_LEN];
    snprintf(cache_key, sizeof(cache_key), "%s_%s", table_name, column_name);

    // Use direct lookup
    return lookup_decltype_direct(pg_conn, cache_key);
}

// ============================================================================
// Helper: Resolve source table names for result columns using PQftable
// ============================================================================
// For queries without AS aliases (e.g., SELECT tags.extra_data FROM tags),
// PostgreSQL returns the column name without table prefix. This function
// uses PQftable() to determine which table each column came from, enabling
// proper decltype cache lookups.
//
// IMPORTANT: Must be called after query execution when result is available.
// Must NOT be called while holding pg_stmt->mutex if it needs to query PG.

// ============================================================================
// PERFORMANCE FIX v0.9.6: Global OID → Table Name Cache
// ============================================================================
// Problem: resolve_column_tables was called 10,000+ times for continueWatching,
// each time doing a PQexec("SELECT oid, relname FROM pg_class WHERE oid IN (...)")
// This caused massive latency (30+ seconds for a single API call).
//
// Solution: Cache OID → table name mappings globally. Table OIDs don't change
// during a session, so we can cache them indefinitely.

#define OID_CACHE_SIZE 512  // Should be enough for all Plex tables

typedef struct {
    Oid oid;
    char name[64];
} oid_cache_entry_t;

static oid_cache_entry_t oid_table_cache[OID_CACHE_SIZE];
static int oid_cache_count = 0;
static pthread_mutex_t oid_cache_mutex = PTHREAD_MUTEX_INITIALIZER;

// Lookup OID in cache - returns table name or NULL if not found
static const char* oid_cache_lookup(Oid oid) {
    // Fast path: check without lock first (read-only, atomic int read)
    int count = oid_cache_count;
    for (int i = 0; i < count && i < OID_CACHE_SIZE; i++) {
        if (oid_table_cache[i].oid == oid) {
            return oid_table_cache[i].name;
        }
    }
    return NULL;
}

// Add OID → name mapping to cache
static void oid_cache_add(Oid oid, const char *name) {
    pthread_mutex_lock(&oid_cache_mutex);
    
    // Check if already exists (race condition check)
    for (int i = 0; i < oid_cache_count && i < OID_CACHE_SIZE; i++) {
        if (oid_table_cache[i].oid == oid) {
            pthread_mutex_unlock(&oid_cache_mutex);
            return;  // Already cached
        }
    }
    
    // Add new entry if space available
    if (oid_cache_count < OID_CACHE_SIZE) {
        oid_table_cache[oid_cache_count].oid = oid;
        strncpy(oid_table_cache[oid_cache_count].name, name, 63);
        oid_table_cache[oid_cache_count].name[63] = '\0';
        oid_cache_count++;
        LOG_DEBUG("OID_CACHE: Added oid=%u -> '%s' (total: %d)", oid, name, oid_cache_count);
    }
    
    pthread_mutex_unlock(&oid_cache_mutex);
}

// Helper: Auto-clear recursion flag on function exit
static inline void resolve_tables_cleanup(int *dummy) {
    (void)dummy;
    in_resolve_tables = 0;
}

int resolve_column_tables(pg_stmt_t *pg_stmt, pg_connection_t *pg_conn) {
    // CRITICAL FIX v0.9.3: Prevent recursion crash
    // Set flag BEFORE any PQexec calls to block shim re-entry
    if (in_resolve_tables) {
        LOG_DEBUG("RESOLVE_TABLES: Recursion detected, aborting");
        if (pg_stmt) pg_stmt->col_tables_resolved = 1;
        return -1;
    }
    in_resolve_tables = 1;
    
    // Auto-clear flag on ANY function exit (GCC/Clang cleanup attribute)
    int cleanup_guard __attribute__((cleanup(resolve_tables_cleanup))) = 0;
    (void)cleanup_guard;
    
    if (!pg_stmt || !pg_stmt->result || pg_stmt->col_tables_resolved) {
        return 0;  // Already resolved or nothing to resolve - success/skip
    }

    int num_cols = pg_stmt->num_cols;
    if (num_cols <= 0 || num_cols > MAX_PARAMS) {
        pg_stmt->col_tables_resolved = 1;
        return 0;  // No columns or too many - skip, not an error
    }

    // PERFORMANCE FIX v0.9.6: First try to resolve ALL columns from cache
    // This avoids PQexec entirely if all OIDs are already cached
    Oid table_oids[MAX_PARAMS];
    int uncached_oids[MAX_PARAMS];
    int num_unique_tables = 0;
    int num_uncached = 0;
    int cache_hits = 0;

    for (int i = 0; i < num_cols; i++) {
        Oid table_oid = PQftable(pg_stmt->result, i);
        if (table_oid == InvalidOid) {
            continue;  // Computed column, no source table
        }

        // Check cache first
        const char *cached_name = oid_cache_lookup(table_oid);
        if (cached_name) {
            // Cache hit! Assign directly without PQexec
            pg_stmt->col_table_names[i] = strdup(cached_name);
            cache_hits++;
            continue;
        }

        // Check if we already have this OID in uncached list
        int found = 0;
        for (int j = 0; j < num_unique_tables; j++) {
            if (table_oids[j] == table_oid) {
                found = 1;
                break;
            }
        }
        if (!found && num_unique_tables < MAX_PARAMS) {
            table_oids[num_unique_tables] = table_oid;
            uncached_oids[num_uncached++] = num_unique_tables;
            num_unique_tables++;
        }
    }

    // If all OIDs were cached, we're done - no PQexec needed!
    if (num_uncached == 0) {
        pg_stmt->col_tables_resolved = 1;
        if (cache_hits > 0) {
            LOG_DEBUG("RESOLVE_TABLES: All %d columns resolved from cache (0 queries)", cache_hits);
        }
        return 0;
    }

    // Need to query PostgreSQL for uncached OIDs
    // STACK OVERFLOW FIX v0.9.6: Allocate query buffer on HEAP instead of stack
    char *query = malloc(4096);
    if (!query) {
        LOG_ERROR("RESOLVE_TABLES: malloc failed for query buffer");
        pg_stmt->col_tables_resolved = 1;
        return -1;
    }
    
    int offset = snprintf(query, 4096,
        "SELECT oid, relname FROM pg_class WHERE oid IN (");

    for (int i = 0; i < num_unique_tables; i++) {
        if (i > 0) {
            offset += snprintf(query + offset, 4096 - offset, ",");
        }
        offset += snprintf(query + offset, 4096 - offset, "%u", table_oids[i]);
    }
    snprintf(query + offset, 4096 - offset, ")");

    // Execute query to get table names (need connection)
    if (!pg_conn || !pg_conn->conn) {
        LOG_DEBUG("RESOLVE_TABLES: No connection available");
        free(query);
        pg_stmt->col_tables_resolved = 1;
        return -1;
    }

    pthread_mutex_lock(&pg_conn->mutex);
    PGresult *res = PQexec(pg_conn->conn, query);
    pthread_mutex_unlock(&pg_conn->mutex);
    
    // Query buffer no longer needed after PQexec
    free(query);
    query = NULL;

    if (!res || PQresultStatus(res) != PGRES_TUPLES_OK) {
        LOG_ERROR("RESOLVE_TABLES: Query failed: %s",
                  res ? PQerrorMessage(pg_conn->conn) : "NULL result");
        if (res) PQclear(res);
        pg_stmt->col_tables_resolved = 1;
        return -1;
    }

    // Build OID -> name map and add to cache
    int num_results = PQntuples(res);
    
    // STACK OVERFLOW FIX v0.9.6: Allocate result_names on HEAP instead of stack
    Oid result_oids[MAX_PARAMS];  // 1KB on stack - acceptable
    char (*result_names)[64] = malloc(MAX_PARAMS * 64);
    if (!result_names) {
        LOG_ERROR("RESOLVE_TABLES: malloc failed for result_names buffer");
        PQclear(res);
        pg_stmt->col_tables_resolved = 1;
        return -1;
    }

    for (int i = 0; i < num_results && i < MAX_PARAMS; i++) {
        result_oids[i] = (Oid)atol(PQgetvalue(res, i, 0));
        strncpy(result_names[i], PQgetvalue(res, i, 1), 63);
        result_names[i][63] = '\0';
        
        // PERFORMANCE FIX v0.9.6: Add to global cache for future queries
        oid_cache_add(result_oids[i], result_names[i]);
    }
    PQclear(res);

    // Now assign table names to columns that weren't resolved from cache
    for (int i = 0; i < num_cols && i < MAX_PARAMS; i++) {
        if (pg_stmt->col_table_names[i]) {
            continue;  // Already resolved from cache
        }
        
        Oid table_oid = PQftable(pg_stmt->result, i);
        if (table_oid == InvalidOid) {
            continue;  // Computed column
        }

        // Find matching table name from query results
        for (int j = 0; j < num_results; j++) {
            if (result_oids[j] == table_oid) {
                pg_stmt->col_table_names[i] = strdup(result_names[j]);
                LOG_DEBUG("RESOLVE_TABLES: col[%d] '%s' -> table '%s'",
                          i, PQfname(pg_stmt->result, i), result_names[j]);
                break;
            }
        }
    }
    
    // Cleanup heap allocation
    free(result_names);

    pg_stmt->col_tables_resolved = 1;
    LOG_INFO("RESOLVE_TABLES: Resolved %d columns (%d from cache, %d from query)",
             num_cols, cache_hits, num_unique_tables);
    return 0;
}

// ============================================================================
// Helper: Decode PostgreSQL hex-encoded BYTEA to binary
// ============================================================================

// PostgreSQL BYTEA hex format: \x followed by hex digits (2 per byte)
// Returns decoded data and sets out_length. Caller must NOT free the result.
const void* pg_decode_bytea(pg_stmt_t *pg_stmt, int row, int col, int *out_length) {
    const char *hex_str = PQgetvalue(pg_stmt->result, row, col);
    if (!hex_str) {
        *out_length = 0;
        return NULL;
    }

    // Check for hex format: starts with \x
    if (hex_str[0] != '\\' || hex_str[1] != 'x') {
        // Not hex format, return raw data (escape format or other)
        *out_length = PQgetlength(pg_stmt->result, row, col);
        return hex_str;
    }

    // Skip \x prefix
    hex_str += 2;
    size_t hex_len = strlen(hex_str);
    size_t bin_len = hex_len / 2;

    // Check if we already have this row cached
    if (pg_stmt->decoded_blob_row == row && pg_stmt->decoded_blobs[col]) {
        *out_length = pg_stmt->decoded_blob_lens[col];
        return pg_stmt->decoded_blobs[col];
    }

    // Clear old cache if row changed
    if (pg_stmt->decoded_blob_row != row) {
        for (int i = 0; i < MAX_PARAMS; i++) {
            if (pg_stmt->decoded_blobs[i]) {
                free(pg_stmt->decoded_blobs[i]);
                pg_stmt->decoded_blobs[i] = NULL;
                pg_stmt->decoded_blob_lens[i] = 0;
            }
        }
        pg_stmt->decoded_blob_row = row;
    }

    // Allocate and decode
    unsigned char *binary = malloc(bin_len + 1);  // +1 for safety
    if (!binary) {
        *out_length = 0;
        return NULL;
    }

    // Inline hex decode - 4-10x faster than sscanf
    // Lookup table for hex digit values (255 = invalid)
    static const unsigned char hex_lut[256] = {
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        0,1,2,3,4,5,6,7,8,9,255,255,255,255,255,255,  // 0-9
        255,10,11,12,13,14,15,255,255,255,255,255,255,255,255,255,  // A-F
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,10,11,12,13,14,15,255,255,255,255,255,255,255,255,255,  // a-f
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
        255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255
    };

    for (size_t i = 0; i < bin_len; i++) {
        unsigned char hi = hex_lut[(unsigned char)hex_str[i * 2]];
        unsigned char lo = hex_lut[(unsigned char)hex_str[i * 2 + 1]];
        if (hi == 255 || lo == 255) {
            free(binary);
            *out_length = 0;
            return NULL;
        }
        binary[i] = (hi << 4) | lo;
    }

    // Cache the decoded data
    pg_stmt->decoded_blobs[col] = binary;
    pg_stmt->decoded_blob_lens[col] = (int)bin_len;
    *out_length = (int)bin_len;

    return binary;
}

// ============================================================================
// Helper: Execute query on-demand for column metadata access
// ============================================================================
// SQLite allows column_count/column_name to be called before step().
// PostgreSQL requires executing the query to get column metadata.
// This helper executes the query if not yet executed.
static int ensure_pg_result_for_metadata(pg_stmt_t *pg_stmt) {
    // Must be called with pg_stmt->mutex held
    if (pg_stmt->result || pg_stmt->cached_result) {
        return 1;  // Already have result
    }
    if (!pg_stmt->pg_sql || !pg_stmt->conn || !pg_stmt->conn->conn) {
        return 0;  // Can't execute - missing query or connection
    }

    // v0.9.4.5: Only execute on PostgreSQL for library.db
    // Non-library databases (blobs.db, etc.) should use SQLite fallback
    if (!is_library_db_path(pg_stmt->conn->db_path)) {
        return 0;  // Not library DB - let caller fall back to SQLite
    }

    // Get the connection to use (thread-local for library DB)
    pg_connection_t *exec_conn = pg_stmt->conn;
    pg_connection_t *thread_conn = pg_get_thread_connection(pg_stmt->conn->db_path);
    if (thread_conn && thread_conn->is_pg_active && thread_conn->conn) {
        exec_conn = thread_conn;
    }

    LOG_INFO("METADATA_EXEC: Executing query for column metadata access: %.100s", pg_stmt->pg_sql);

    // Lock the connection mutex
    pthread_mutex_lock(&exec_conn->mutex);

    // Drain any pending results
    PQsetnonblocking(exec_conn->conn, 0);
    while (PQisBusy(exec_conn->conn)) {
        PQconsumeInput(exec_conn->conn);
    }
    PGresult *pending;
    while ((pending = PQgetResult(exec_conn->conn)) != NULL) {
        PQclear(pending);
    }

    // Build parameter values array
    const char *paramValues[MAX_PARAMS] = {NULL};
    for (int i = 0; i < pg_stmt->param_count && i < MAX_PARAMS; i++) {
        paramValues[i] = pg_stmt->param_values[i];
    }

    // Execute the query
    pg_stmt->result = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
                                    pg_stmt->param_count, NULL,
                                    paramValues, NULL, NULL, 0);
    pthread_mutex_unlock(&exec_conn->mutex);

    if (PQresultStatus(pg_stmt->result) == PGRES_TUPLES_OK) {
        pg_stmt->num_rows = PQntuples(pg_stmt->result);
        pg_stmt->num_cols = PQnfields(pg_stmt->result);
        pg_stmt->current_row = -1;  // Will be 0 after first step()
        pg_stmt->result_conn = exec_conn;

        // Resolve source table names for bare column lookup in decltype
        // This enables proper type lookups for queries without AS aliases
        if (resolve_column_tables(pg_stmt, exec_conn) < 0) {
            LOG_ERROR("Failed to resolve column tables");
        }

        // v0.8.9 FIX: Mark this result as from metadata-only execution
        // If bind() is called later, we need to re-execute with bound params
        pg_stmt->metadata_only_result = 1;

        LOG_INFO("METADATA_EXEC: Success - %d cols, %d rows (metadata_only=1)", pg_stmt->num_cols, pg_stmt->num_rows);
        return 1;
    } else {
        LOG_ERROR("METADATA_EXEC: Query failed: %s", PQerrorMessage(exec_conn->conn));
        PQclear(pg_stmt->result);
        pg_stmt->result = NULL;
        return 0;
    }
}

// ============================================================================
// Column Functions
// ============================================================================

int my_sqlite3_column_count(sqlite3_stmt *pStmt) {
    LOG_DEBUG("COLUMN_COUNT: stmt=%p", (void*)pStmt);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    // Handle both READ (is_pg == 2) and WRITE (is_pg == 1) statements
    // For WRITE without RETURNING result, return 0 columns (no data to read)
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            int count = pg_stmt->cached_result->num_cols;
            pthread_mutex_unlock(&pg_stmt->mutex);
            return count;
        }
        // If num_cols is 0 and we have a query but no result yet,
        // execute the query to get column metadata (SQLite allows this before step)
        if (pg_stmt->num_cols == 0 && pg_stmt->pg_sql && !pg_stmt->result) {
            ensure_pg_result_for_metadata(pg_stmt);
        }
        // For PostgreSQL statements, return our stored num_cols
        // Don't fall through to orig_sqlite3_column_count which would fail
        // because pStmt is not a valid SQLite statement
        int count = pg_stmt->num_cols;
        pthread_mutex_unlock(&pg_stmt->mutex);
        return count;
    }
    return orig_sqlite3_column_count ? orig_sqlite3_column_count(pStmt) : 0;
}

// Helper to convert SQLite type to string for logging
static const char* sqlite_type_name(int type) {
    switch (type) {
        case SQLITE_INTEGER: return "INTEGER";
        case SQLITE_FLOAT: return "FLOAT";
        case SQLITE_TEXT: return "TEXT";
        case SQLITE_BLOB: return "BLOB";
        case SQLITE_NULL: return "NULL";
        default: return "UNKNOWN";
    }
}

// ---------------------------------------------------------------------------
// Targeted bad_cast tracing
//
// Goal: capture the exact decltype/type values returned for specific column
// indexes right before SOCI throws std::bad_cast.
//
// Enable with:
//   PLEX_PG_TRACE_BADCAST=1
// Optional:
//   PLEX_PG_TRACE_BADCAST_IDX="5,6"   (default)
//   PLEX_PG_TRACE_BADCAST_THREAD="ReqHandler" (default; set to "any" to disable)
// ---------------------------------------------------------------------------

static int trace_badcast_enabled = -1;
static const char *trace_badcast_idx_list = NULL;
static const char *trace_badcast_thread_substr = NULL;
static const char *trace_badcast_sql_contains = NULL;
static const char *trace_badcast_col_contains = NULL;

static char trace_badcast_idx_file_buf[256];
static char trace_badcast_sql_file_buf[256];
static char trace_badcast_thread_file_buf[128];
static char trace_badcast_col_file_buf[128];

static const char *read_trace_file_first_line(const char *path, char *buf, size_t buf_len) {
    if (!path || !buf || buf_len < 2) return NULL;
    buf[0] = '\0';

    FILE *f = fopen(path, "r");
    if (!f) return NULL;
    if (!fgets(buf, (int)buf_len, f)) {
        fclose(f);
        return NULL;
    }
    fclose(f);

    // Trim trailing newline/CR and surrounding whitespace.
    size_t n = strlen(buf);
    while (n > 0 && (buf[n - 1] == '\n' || buf[n - 1] == '\r' || buf[n - 1] == ' ' || buf[n - 1] == '\t')) {
        buf[--n] = '\0';
    }
    char *p = buf;
    while (*p == ' ' || *p == '\t') p++;
    if (p != buf) memmove(buf, p, strlen(p) + 1);

    return buf[0] ? buf : NULL;
}

static void trace_badcast_log_ctx(pg_stmt_t *pg_stmt,
                                 sqlite3_stmt *pStmt,
                                 int idx,
                                 const char *fn,
                                 const char *phase,
                                 int row,
                                 int is_null,
                                 Oid oid,
                                 const char *col_name) {
    if (!pg_stmt) return;
    // Keep this line compact and safe to log (no large values).
    // Use LOG_ERROR so it still appears even if log level is ERROR.
    LOG_ERROR("TRACE_BADCAST_CTX: fn=%s phase=%s stmt=%p pg_stmt=%p idx=%d col='%s' oid=%u row=%d/%d cols=%d is_null=%d sql=%.200s",
             fn ? fn : "?", phase ? phase : "?",
             (void*)pStmt, (void*)pg_stmt, idx,
             col_name ? col_name : "?", (unsigned)oid,
             row, pg_stmt->num_rows, pg_stmt->num_cols,
             is_null,
             pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
}

static void trace_badcast_init(void) {
    if (trace_badcast_enabled != -1) return;

    const char *env = getenv("PLEX_PG_TRACE_BADCAST");
    if (env) {
        trace_badcast_enabled = (env[0] && strcmp(env, "0") != 0) ? 1 : 0;
    } else {
        // If the var isn't present (some Plex launch paths sanitize env),
        // auto-enable tracing when log level is ERROR. This keeps behavior
        // opt-in for normal DEBUG/INFO runs.
        const char *lvl = getenv("PLEX_PG_LOG_LEVEL");
        trace_badcast_enabled = (lvl && strcasecmp(lvl, "ERROR") == 0) ? 1 : 0;
    }

    trace_badcast_idx_list = getenv("PLEX_PG_TRACE_BADCAST_IDX");
    if (!trace_badcast_idx_list || !trace_badcast_idx_list[0]) {
        // Some Plex restart paths sanitize env. Allow /tmp overrides.
        trace_badcast_idx_list = read_trace_file_first_line(
            "/tmp/plex_pg_trace_badcast_idx",
            trace_badcast_idx_file_buf,
            sizeof(trace_badcast_idx_file_buf)
        );
    }
    if (!trace_badcast_idx_list || !trace_badcast_idx_list[0]) trace_badcast_idx_list = "5,6";

    trace_badcast_thread_substr = getenv("PLEX_PG_TRACE_BADCAST_THREAD");
    if (!trace_badcast_thread_substr || !trace_badcast_thread_substr[0]) {
        trace_badcast_thread_substr = read_trace_file_first_line(
            "/tmp/plex_pg_trace_badcast_thread",
            trace_badcast_thread_file_buf,
            sizeof(trace_badcast_thread_file_buf)
        );
    }
    if (!trace_badcast_thread_substr || !trace_badcast_thread_substr[0]) {
        // Default to no thread filter.
        trace_badcast_thread_substr = "any";
    }

    trace_badcast_sql_contains = getenv("PLEX_PG_TRACE_BADCAST_SQL_CONTAINS");
    if (!trace_badcast_sql_contains || !trace_badcast_sql_contains[0]) {
        trace_badcast_sql_contains = read_trace_file_first_line(
            "/tmp/plex_pg_trace_badcast_sql_contains",
            trace_badcast_sql_file_buf,
            sizeof(trace_badcast_sql_file_buf)
        );
    }

    trace_badcast_col_contains = getenv("PLEX_PG_TRACE_BADCAST_COL_CONTAINS");
    if (!trace_badcast_col_contains || !trace_badcast_col_contains[0]) {
        trace_badcast_col_contains = read_trace_file_first_line(
            "/tmp/plex_pg_trace_badcast_col_contains",
            trace_badcast_col_file_buf,
            sizeof(trace_badcast_col_file_buf)
        );
    }
}

static int trace_badcast_list_contains_idx(const char *list, int idx) {
    if (!list || !list[0]) return 0;
    if (strcasecmp(list, "all") == 0) return 1;

    const char *p = list;
    while (*p) {
        // Skip separators
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == ',' || *p == ';') p++;
        if (!*p) break;

        char *end = NULL;
        long v = strtol(p, &end, 10);
        if (end == p) {
            // Skip non-numeric token
            while (*p && *p != ',' && *p != ';' && *p != ' ' && *p != '\t' && *p != '\n') p++;
            continue;
        }
        if ((int)v == idx) return 1;
        p = end;
    }
    return 0;
}

static int trace_badcast_thread_ok(void) {
    trace_badcast_init();
    if (!trace_badcast_enabled) return 0;
    if (!trace_badcast_thread_substr || !trace_badcast_thread_substr[0]) return 1;
    if (strcasecmp(trace_badcast_thread_substr, "any") == 0) return 1;

#ifdef __APPLE__
    char tname[64];
    tname[0] = '\0';
    pthread_getname_np(pthread_self(), tname, sizeof(tname));
    if (!tname[0]) return 0;
    return strstr(tname, trace_badcast_thread_substr) != NULL;
#else
    // Non-Apple platforms: no thread name filter.
    return 1;
#endif
}

static int trace_badcast_sql_ok(const pg_stmt_t *pg_stmt) {
    trace_badcast_init();
    if (!trace_badcast_enabled) return 0;
    if (!trace_badcast_sql_contains || !trace_badcast_sql_contains[0]) return 1;
    if (!pg_stmt || !pg_stmt->pg_sql) return 0;

    // Comma/semicolon separated list of substrings; any match enables logging.
    const char *sql = pg_stmt->pg_sql;
    const char *p = trace_badcast_sql_contains;
    while (*p) {
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == ',' || *p == ';') p++;
        if (!*p) break;
        const char *start = p;
        while (*p && *p != ',' && *p != ';' && *p != '\n') p++;
        size_t len = (size_t)(p - start);
        if (len > 0) {
            // Temporary NUL-terminated token without heap alloc.
            char token[128];
            if (len >= sizeof(token)) len = sizeof(token) - 1;
            memcpy(token, start, len);
            token[len] = '\0';
            if (token[0] && strstr(sql, token) != NULL) return 1;
        }
    }
    return 0;
}

static int trace_badcast_col_ok(const char *col_name) {
    trace_badcast_init();
    if (!trace_badcast_enabled) return 0;
    if (!trace_badcast_col_contains || !trace_badcast_col_contains[0]) return 1;
    if (!col_name || !col_name[0]) return 0;

    // Comma/semicolon separated list of substrings; any match enables logging.
    const char *p = trace_badcast_col_contains;
    while (*p) {
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == ',' || *p == ';') p++;
        if (!*p) break;
        const char *start = p;
        while (*p && *p != ',' && *p != ';' && *p != '\n') p++;
        size_t len = (size_t)(p - start);
        if (len > 0) {
            char token[64];
            if (len >= sizeof(token)) len = sizeof(token) - 1;
            memcpy(token, start, len);
            token[len] = '\0';
            if (token[0] && strstr(col_name, token) != NULL) return 1;
        }
    }
    return 0;
}

static int trace_badcast_should_log(const pg_stmt_t *pg_stmt, int idx) {
    trace_badcast_init();
    if (!trace_badcast_enabled) return 0;
    if (!trace_badcast_thread_ok()) return 0;
    if (!trace_badcast_sql_ok(pg_stmt)) return 0;
    return trace_badcast_list_contains_idx(trace_badcast_idx_list, idx);
}

static int trace_badcast_should_log_col(const pg_stmt_t *pg_stmt, int idx, const char *col_name) {
    if (!trace_badcast_should_log(pg_stmt, idx)) return 0;
    return trace_badcast_col_ok(col_name);
}

// Plex related-items queries include collection/folder rows (metadata_type=18)
// but then dynamic_cast them to Show/Episode and crash with std::bad_cast.
// We mask type 18 only for the known "related" query shape so other code paths
// (e.g. SyncCollections) can still read it normally.
static int is_related_items_query(const pg_stmt_t *pg_stmt) {
    if (!pg_stmt || !pg_stmt->pg_sql) return 0;
    return strstr(pg_stmt->pg_sql, "taggings as related") != NULL;
}

// Check if a metadata_type column value should be masked.
// Returns 1 (and sets *out to the masked value) if masking applies, 0 otherwise.
static int mask_collection_metadata_type(const pg_stmt_t *pg_stmt, const char *col_name,
                                         long long raw_val, long long *out) {
    if (raw_val != 18) return 0;
    if (!col_name || !strstr(col_name, "metadata_type")) return 0;
    if (!is_related_items_query(pg_stmt)) return 0;
    LOG_DEBUG("COMPAT_TYPE18: masking metadata_type 18 -> 0 for related-items query, row %d",
              pg_stmt->current_row);
    *out = 0;
    return 1;
}

// Type consistency validation helper
// Validates that column_type and column_decltype are consistent
static void validate_type_consistency(sqlite3_stmt *pStmt, int idx, const char *accessor_name) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    if (!pg_stmt || !pg_stmt->is_pg) return;
    
    int col_type = my_sqlite3_column_type(pStmt, idx);
    const char *col_decltype = my_sqlite3_column_decltype(pStmt, idx);
    
    pthread_mutex_lock(&pg_stmt->mutex);
    if (!pg_stmt->result) {
        pthread_mutex_unlock(&pg_stmt->mutex);
        return;
    }
    
    Oid oid = PQftype(pg_stmt->result, idx);
    const char *col_name = PQfname(pg_stmt->result, idx);
    
    // Warn about type mismatches
    if (col_decltype) {
            int expected_for_decltype = -1;
            if (strcmp(col_decltype, "INTEGER") == 0 || 
                strcmp(col_decltype, "BIGINT") == 0 ||
                strcmp(col_decltype, "dt_integer(8)") == 0) {
                expected_for_decltype = SQLITE_INTEGER;
            } else if (strcmp(col_decltype, "TEXT") == 0) {
                expected_for_decltype = SQLITE_TEXT;
            } else if (strcmp(col_decltype, "REAL") == 0) {
                expected_for_decltype = SQLITE_FLOAT;
            } else if (strcmp(col_decltype, "BLOB") == 0) {
                expected_for_decltype = SQLITE_BLOB;
            }
        
        if (expected_for_decltype != -1 && col_type != SQLITE_NULL && col_type != expected_for_decltype) {
            LOG_DEBUG("TYPE_MISMATCH: accessor=%s col='%s' idx=%d decltype='%s' expects %s but column_type returned %s (OID=%u)",
                      accessor_name, col_name ? col_name : "?", idx,
                      col_decltype, sqlite_type_name(expected_for_decltype),
                      sqlite_type_name(col_type), (unsigned)oid);

            // If bad_cast tracing is active for this idx/sql, emit an ERROR line
            // so we can correlate mismatches with Plex crashes even when running
            // at ERROR log level.
            if (trace_badcast_should_log(pg_stmt, idx)) {
                trace_badcast_log_ctx(pg_stmt, pStmt, idx, accessor_name, "type_mismatch",
                                      pg_stmt->current_row,
                                      (col_type == SQLITE_NULL) ? 1 : 0,
                                      oid,
                                      col_name);
                LOG_ERROR("TRACE_BADCAST_MISMATCH: accessor=%s col='%s' idx=%d oid=%u decltype='%s' expected=%s actual=%s sql=%.200s",
                          accessor_name,
                          col_name ? col_name : "?",
                          idx,
                          (unsigned)oid,
                          col_decltype,
                          sqlite_type_name(expected_for_decltype),
                          sqlite_type_name(col_type),
                          pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            }
        }
    }
    pthread_mutex_unlock(&pg_stmt->mutex);
}

int my_sqlite3_column_type(sqlite3_stmt *pStmt, int idx) {
    global_column_type_calls++;  // Global counter for exception debugging
    LOG_DEBUG("COLUMN_TYPE: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    int trace = 0;
    // Handle all PostgreSQL statements
    // For WRITE without result, return SQLITE_NULL (no data)
    if (pg_stmt && pg_stmt->is_pg) {
        // Update exception context BEFORE locking (query is constant)
        last_query_being_processed = pg_stmt->pg_sql;
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                const char *cname = (cached->col_names && idx < cached->num_cols) ? cached->col_names[idx] : NULL;
                trace = trace_badcast_should_log_col(pg_stmt, idx, cname);
                if (crow->is_null[idx]) {
                    // Return SQLITE_NULL for NULL values.
                    // SOCI's load_rowset() handles this correctly by setting isNull_=true.
                    // SOCI's post_fetch() may throw "Null value not allowed" but Plex catches this.
                    LOG_DEBUG("COLUMN_TYPE_VERBOSE: idx=%d row=%d -> SQLITE_NULL (cached, is_null=true)", 
                              idx, row);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return SQLITE_NULL;
                }

                // Mask collection type 18 -> NULL so SOCI marks isNull_=true and Plex skips the row.
                if (crow->values[idx]) {
                    long long masked;
                    if (mask_collection_metadata_type(pg_stmt, cname, atoi(crow->values[idx]), &masked)) {
                        pthread_mutex_unlock(&pg_stmt->mutex);
                        return SQLITE_NULL;
                    }
                }

                // Use cached column type OID to determine SQLite type
                Oid oid = cached->col_types[idx];
                int result = pg_oid_to_sqlite_type(oid);
                if (trace) {
                    // We don't have a PGresult here; still log a consistent context line.
                    trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_type", "cached", row, 0, oid, cname);
                    LOG_ERROR("TRACE_BADCAST: column_type (cached) idx=%d col='%s' row=%d oid=%u -> %s sql=%.200s",
                             idx, cname ? cname : "?", row, (unsigned)oid,
                             sqlite_type_name(result), pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
                }
                LOG_DEBUG("COLUMN_TYPE_VERBOSE: idx=%d row=%d OID=%u -> %s (cached)",
                        idx, row, (unsigned)oid, sqlite_type_name(result));
                pthread_mutex_unlock(&pg_stmt->mutex);
                return result;
            }
            LOG_DEBUG("COLUMN_TYPE_VERBOSE: idx=%d row=%d -> SQLITE_NULL (cached, out of bounds)", idx, row);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }

        if (!pg_stmt->result) {
            LOG_DEBUG("COLUMN_TYPE_VERBOSE: idx=%d -> SQLITE_NULL (no result)", idx);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_DEBUG("COL_TYPE_BOUNDS: idx=%d out of bounds (num_cols=%d) sql=%.100s",
                     idx, pg_stmt->num_cols, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_DEBUG("COL_TYPE_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }
        int is_null = PQgetisnull(pg_stmt->result, row, idx);
        Oid oid = PQftype(pg_stmt->result, idx);
        const char *col_name = PQfname(pg_stmt->result, idx);
        trace = trace_badcast_should_log_col(pg_stmt, idx, col_name);
        // Update exception context
        last_column_being_accessed = col_name;
        // Return SQLITE_NULL for NULL values.
        if (is_null) {
            LOG_DEBUG("COLUMN_TYPE: idx=%d col='%s' is NULL, returning SQLITE_NULL",
                      idx, col_name ? col_name : "?");
            if (trace) {
                trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_type", "live", row, 1, oid, col_name);
                LOG_ERROR("TRACE_BADCAST: column_type idx=%d col='%s' row=%d oid=%u is_null=1 -> NULL sql=%.200s",
                         idx, col_name ? col_name : "?", row, (unsigned)oid,
                         pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return SQLITE_NULL;
        }

        // Mask collection type 18 -> NULL so SOCI marks isNull_=true and Plex skips the row.
        {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            long long masked;
            if (val && mask_collection_metadata_type(pg_stmt, col_name, atoi(val), &masked)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return SQLITE_NULL;
            }
        }

        int result = pg_oid_to_sqlite_type(oid);
        
        // ENHANCED LOGGING: Include decltype for comparison
        const char *col_decltype = NULL;
        // Quick decltype check without recursive call - use OID mapping
        switch (oid) {
            case 16: case 21: case 23: case 26:  // bool, int2, int4, oid
                col_decltype = "INTEGER";
                break;
            case 20:  // int8
                col_decltype = "BIGINT";
                break;
            case 700: case 701: case 1700:  // float4, float8, numeric
                col_decltype = "REAL";
                break;
            case 17:  // bytea
                col_decltype = "BLOB";
                break;
            default:
                col_decltype = "TEXT";
                break;
        }
        
        if (trace) {
            trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_type", "live", row, 0, oid, col_name);
            LOG_ERROR("TRACE_BADCAST: column_type idx=%d col='%s' row=%d oid=%u is_null=0 -> %s (guess_decltype='%s') sql=%.200s",
                     idx, col_name ? col_name : "?", row, (unsigned)oid,
                     sqlite_type_name(result), col_decltype ? col_decltype : "?",
                     pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
        }
        LOG_DEBUG("COLUMN_TYPE: idx=%d col='%s' row=%d OID=%u is_null=%d -> %s (decltype='%s')",
                idx, col_name ? col_name : "?", row, (unsigned)oid, is_null,
                sqlite_type_name(result), col_decltype ? col_decltype : "NULL");
        pthread_mutex_unlock(&pg_stmt->mutex);
        return result;
    }
    return orig_sqlite3_column_type ? orig_sqlite3_column_type(pStmt, idx) : SQLITE_NULL;
}

int my_sqlite3_column_int(sqlite3_stmt *pStmt, int idx) {
    validate_type_consistency(pStmt, idx, "column_int");
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    const int trace = trace_badcast_should_log(pg_stmt, idx);
    
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    const char *val = crow->values[idx];
                    int result_val = 0;
                    if (val[0] == 't' && val[1] == '\0') result_val = 1;
                    else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
                    else result_val = atoi(val);
                    
                    // TYPE_DEBUG: Enhanced logging for type-related columns (cached path)
                    const char *col_name = (idx < MAX_PARAMS && cached->col_names) ? cached->col_names[idx] : NULL;
                    if (col_name && strstr(col_name, "type") != NULL) {
                        LOG_DEBUG("TYPE_DEBUG_CACHED: col='%s' idx=%d row=%d raw_val='%s' result=%d sql=%.200s",
                                  col_name, idx, row, val, result_val,
                                  pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
                    }
                    
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return result_val;
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_DEBUG("COL_INT_BOUNDS: idx=%d out of bounds (num_cols=%d)", idx, pg_stmt->num_cols);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_DEBUG("COL_INT_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        int result_val = 0;
        const char *val = NULL;
        const char *col_name = PQfname(pg_stmt->result, idx);
        Oid oid = PQftype(pg_stmt->result, idx);

        if (trace && oid == 20) {
            trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_int", "entry", row, 0, oid, col_name);
            LOG_ERROR("TRACE_BADCAST_ACCESSOR: column_int called for oid=20 col='%s' idx=%d sql=%.200s",
                      col_name ? col_name : "?", idx, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
        }
        
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') result_val = 1;
            else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
            else result_val = atoi(val);
            
            {
                long long masked;
                if (mask_collection_metadata_type(pg_stmt, col_name, result_val, &masked))
                    result_val = (int)masked;
            }
        }
        
        // TYPE_DEBUG: Enhanced logging for type-related columns (non-cached path)
        if (col_name && strstr(col_name, "type") != NULL) {
            LOG_DEBUG("TYPE_DEBUG: col='%s' idx=%d row=%d raw_val='%s' result=%d sql=%.200s",
                      col_name, idx, row, val ? val : "(NULL)", result_val,
                      pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
        }
        
        pthread_mutex_unlock(&pg_stmt->mutex);
        return result_val;
    }
    return orig_sqlite3_column_int ? orig_sqlite3_column_int(pStmt, idx) : 0;
}

sqlite3_int64 my_sqlite3_column_int64(sqlite3_stmt *pStmt, int idx) {
    validate_type_consistency(pStmt, idx, "column_int64");
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    const int trace = trace_badcast_should_log(pg_stmt, idx);
    
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    const char *val = crow->values[idx];
                    sqlite3_int64 result_val = 0;
                    if (val[0] == 't' && val[1] == '\0') result_val = 1;
                    else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
                    else result_val = atoll(val);

                    {
                        const char *col_name = (idx < MAX_PARAMS && cached->col_names) ? cached->col_names[idx] : NULL;
                        long long masked;
                        if (mask_collection_metadata_type(pg_stmt, col_name, result_val, &masked))
                            result_val = masked;
                    }

                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return result_val;
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_DEBUG("COL_INT64_BOUNDS: idx=%d out of bounds (num_cols=%d)", idx, pg_stmt->num_cols);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            LOG_DEBUG("COL_INT64_ROW_BOUNDS: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        sqlite3_int64 result_val = 0;
        const char *col_name = PQfname(pg_stmt->result, idx);
        Oid oid = PQftype(pg_stmt->result, idx);

        if (trace && oid == 20) {
            trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_int64", "entry", row, 0, oid, col_name);
            LOG_ERROR("TRACE_BADCAST_ACCESSOR: column_int64 called for oid=20 col='%s' idx=%d sql=%.200s",
                      col_name ? col_name : "?", idx, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
        }

        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') result_val = 1;
            else if (val[0] == 'f' && val[1] == '\0') result_val = 0;
            else result_val = atoll(val);

            {
                long long masked;
                if (mask_collection_metadata_type(pg_stmt, col_name, result_val, &masked))
                    result_val = masked;
            }
            
            // TYPE_DEBUG: Enhanced logging for type-related columns (non-cached path)
            if (col_name && strstr(col_name, "type") != NULL) {
                LOG_DEBUG("TYPE_DEBUG_INT64: col='%s' idx=%d row=%d raw_val='%s' result=%lld sql=%.200s",
                          col_name, idx, row, val ? val : "(NULL)", (long long)result_val,
                          pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            }
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return result_val;
    }
    return orig_sqlite3_column_int64 ? orig_sqlite3_column_int64(pStmt, idx) : 0;
}

double my_sqlite3_column_double(sqlite3_stmt *pStmt, int idx) {
    validate_type_consistency(pStmt, idx, "column_double");
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    const char *val = crow->values[idx];
                    double result_val = 0.0;
                    if (val[0] == 't' && val[1] == '\0') result_val = 1.0;
                    else if (val[0] == 'f' && val[1] == '\0') result_val = 0.0;
                    else result_val = atof(val);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return result_val;
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0.0;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0.0;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0.0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0.0;
        }
        double result_val = 0.0;
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            const char *val = PQgetvalue(pg_stmt->result, row, idx);
            if (val[0] == 't' && val[1] == '\0') result_val = 1.0;
            else if (val[0] == 'f' && val[1] == '\0') result_val = 0.0;
            else result_val = atof(val);
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return result_val;
    }
    return orig_sqlite3_column_double ? orig_sqlite3_column_double(pStmt, idx) : 0.0;
}

// ============================================================================
// UTF-8 Validation and String Sanitization
// ============================================================================
// Boost.Locale in Plex may be sensitive to invalid UTF-8 sequences.
// This function validates and optionally sanitizes strings from PostgreSQL.

static int is_valid_utf8_char(const unsigned char *s, size_t len, size_t *char_len) {
    if (len == 0) return 0;
    
    unsigned char c = s[0];
    
    // ASCII (0xxxxxxx)
    if (c <= 0x7F) {
        *char_len = 1;
        return 1;
    }
    
    // 2-byte UTF-8 (110xxxxx 10xxxxxx)
    if ((c & 0xE0) == 0xC0) {
        if (len < 2) return 0;
        if ((s[1] & 0xC0) != 0x80) return 0;
        // Check for overlong encoding
        if (c < 0xC2) return 0;
        *char_len = 2;
        return 1;
    }
    
    // 3-byte UTF-8 (1110xxxx 10xxxxxx 10xxxxxx)
    if ((c & 0xF0) == 0xE0) {
        if (len < 3) return 0;
        if ((s[1] & 0xC0) != 0x80) return 0;
        if ((s[2] & 0xC0) != 0x80) return 0;
        // Check for overlong encoding and surrogates
        if (c == 0xE0 && s[1] < 0xA0) return 0;
        if (c == 0xED && s[1] >= 0xA0) return 0; // Reject surrogates U+D800..U+DFFF
        *char_len = 3;
        return 1;
    }
    
    // 4-byte UTF-8 (11110xxx 10xxxxxx 10xxxxxx 10xxxxxx)
    if ((c & 0xF8) == 0xF0) {
        if (len < 4) return 0;
        if ((s[1] & 0xC0) != 0x80) return 0;
        if ((s[2] & 0xC0) != 0x80) return 0;
        if ((s[3] & 0xC0) != 0x80) return 0;
        // Check for overlong encoding and values > U+10FFFF
        if (c == 0xF0 && s[1] < 0x90) return 0;
        if (c == 0xF4 && s[1] >= 0x90) return 0;
        if (c >= 0xF5) return 0;
        *char_len = 4;
        return 1;
    }
    
    return 0; // Invalid UTF-8 start byte
}

static int validate_utf8_string(const char *str, size_t len) {
    size_t i = 0;
    while (i < len) {
        size_t char_len;
        if (!is_valid_utf8_char((const unsigned char *)(str + i), len - i, &char_len)) {
            return 0; // Invalid UTF-8 sequence found
        }
        i += char_len;
    }
    return 1; // Valid UTF-8
}

// Static buffers for column_text - INCREASED SIZE v0.8.13
// Due to potential Boost.Locale issues, we copy strings instead of returning PQgetvalue() directly.
// Use larger buffers and more of them to handle concurrent access patterns.
#define NUM_TEXT_BUFFERS 64
#define TEXT_BUFFER_SIZE 8192
static __thread char column_text_buffers[NUM_TEXT_BUFFERS][TEXT_BUFFER_SIZE];
static __thread int column_text_buf_idx = 0;  // Thread-local, no atomic needed

// Rewrite server://<machineId>/com.plexapp.plugins.library/library/... to library://...
// Handles both standalone URIs and JSON-embedded URIs (e.g. inside pv:uri in extra_data).
// Output never exceeds input length. Returns 1 if any rewrite was performed.
static int rewrite_server_library_uri(const char *in, char *out, size_t out_len) {
    if (!in || !out || out_len < 16) return 0;

    static const char server_prefix[] = "server://";
    static const size_t server_prefix_len = sizeof(server_prefix) - 1;
    static const char needle[] = "/com.plexapp.plugins.library/library/";
    static const size_t needle_len = sizeof(needle) - 1;
    static const char replacement[] = "library://";
    static const size_t replacement_len = sizeof(replacement) - 1;

    // Quick check: does the string contain "server://" at all?
    if (!strstr(in, server_prefix)) return 0;
    // Does it also contain the plugin path?
    if (!strstr(in, needle)) return 0;

    // Walk the input, copying to output, rewriting each server:// URI match
    size_t in_pos = 0;
    size_t out_pos = 0;
    size_t in_len = strlen(in);
    int rewrites = 0;

    while (in_pos < in_len) {
        // Look for next "server://" from current position
        const char *match = strstr(in + in_pos, server_prefix);
        if (!match) {
            // No more matches - copy rest of input
            size_t remaining = in_len - in_pos;
            if (out_pos + remaining >= out_len) remaining = out_len - out_pos - 1;
            memcpy(out + out_pos, in + in_pos, remaining);
            out_pos += remaining;
            break;
        }

        // Copy everything before this "server://" match
        size_t prefix_bytes = (size_t)(match - (in + in_pos));
        if (out_pos + prefix_bytes >= out_len) {
            // Not enough space - copy what fits and stop
            size_t fits = out_len - out_pos - 1;
            if (fits > 0) memcpy(out + out_pos, in + in_pos, fits);
            out_pos += fits;
            break;
        }
        memcpy(out + out_pos, in + in_pos, prefix_bytes);
        out_pos += prefix_bytes;
        in_pos += prefix_bytes;

        // Now in_pos points to "server://"
        // Find the needle "/com.plexapp.plugins.library/library/" after the server:// prefix
        // The machineId is between "server://" and the needle
        const char *needle_pos = strstr(in + in_pos + server_prefix_len, needle);
        if (!needle_pos) {
            // No plugin path after this server:// - copy "server://" literally and move on
            size_t copy = server_prefix_len;
            if (out_pos + copy >= out_len) copy = out_len - out_pos - 1;
            memcpy(out + out_pos, in + in_pos, copy);
            out_pos += copy;
            in_pos += server_prefix_len;
            continue;
        }

        // Replace "server://<machineId>/com.plexapp.plugins.library/library/" with "library://"
        size_t full_prefix_len = (size_t)(needle_pos - (in + in_pos)) + needle_len;
        if (out_pos + replacement_len >= out_len) break;
        memcpy(out + out_pos, replacement, replacement_len);
        out_pos += replacement_len;
        in_pos += full_prefix_len;
        rewrites++;
    }

    out[out_pos] = '\0';

    if (rewrites > 0) {
        LOG_DEBUG("URI_REWRITE: rewrote %d server:// -> library:// URIs (in_len=%zu out_len=%zu)",
                  rewrites, in_len, out_pos);
    }
    return rewrites > 0 ? 1 : 0;
}

const unsigned char* my_sqlite3_column_text(sqlite3_stmt *pStmt, int idx) {
    validate_type_consistency(pStmt, idx, "column_text");
    
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    
    // DEBUG: Log when pg_stmt is not found or not PG
    if (!pg_stmt) {
        LOG_DEBUG("COLUMN_TEXT_NO_STMT: pStmt=%p idx=%d - statement not in registry (non-PG db, using SQLite fallback)", (void*)pStmt, idx);
    } else if (!pg_stmt->is_pg) {
        LOG_DEBUG("COLUMN_TEXT_NOT_PG: pStmt=%p idx=%d is_pg=false, using SQLite fallback", (void*)pStmt, idx);
    }
    
    // PERFORMANCE FIX: Use cached flag instead of expensive strstr() on every column access
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);
        
        LOG_DEBUG("COLUMN_TEXT: locked mutex, result=%p row=%d cols=%d",
                 (void*)pg_stmt->result, pg_stmt->current_row, pg_stmt->num_cols);

        const char *source_value = NULL;

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            LOG_DEBUG("COLUMN_TEXT_CACHE: idx=%d row=%d num_cols=%d num_rows=%d",
                     idx, row, cached->num_cols, cached->num_rows);
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    source_value = crow->values[idx];

                    LOG_DEBUG("COLUMN_TEXT_CACHE_HIT: found cached value len=%zu", strlen(source_value));
                }
            }
            if (!source_value) {
                // Return NULL for NULL columns - SQLite behavior
                LOG_DEBUG("COLUMN_TEXT_CACHE_NULL: idx=%d row=%d returning NULL", idx, row);
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }
        } else {
            // Non-cached path - get from PGresult
            if (!pg_stmt->result) {
                LOG_DEBUG("COLUMN_TEXT: no result, returning empty buffer");
                int buf = column_text_buf_idx;
                column_text_buf_idx = (column_text_buf_idx + 1) % NUM_TEXT_BUFFERS;
                column_text_buffers[buf][0] = '\0';
                pthread_mutex_unlock(&pg_stmt->mutex);
                return (const unsigned char*)column_text_buffers[buf];
            }

            if (idx < 0 || idx >= pg_stmt->num_cols) {
                LOG_DEBUG("COLUMN_TEXT: idx=%d out of bounds (num_cols=%d)", idx, pg_stmt->num_cols);
                int buf = column_text_buf_idx;
                column_text_buf_idx = (column_text_buf_idx + 1) % NUM_TEXT_BUFFERS;
                column_text_buffers[buf][0] = '\0';
                pthread_mutex_unlock(&pg_stmt->mutex);
                return (const unsigned char*)column_text_buffers[buf];
            }

            int row = pg_stmt->current_row;
            if (row < 0 || row >= pg_stmt->num_rows) {
                LOG_DEBUG("COLUMN_TEXT: row=%d out of bounds (num_rows=%d)", row, pg_stmt->num_rows);
                int buf = column_text_buf_idx;
                column_text_buf_idx = (column_text_buf_idx + 1) % NUM_TEXT_BUFFERS;
                column_text_buffers[buf][0] = '\0';
                pthread_mutex_unlock(&pg_stmt->mutex);
                return (const unsigned char*)column_text_buffers[buf];
            }

            if (PQgetisnull(pg_stmt->result, row, idx)) {
                // Return NULL for NULL columns - SQLite behavior
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }

            source_value = PQgetvalue(pg_stmt->result, row, idx);
            if (!source_value) {
                LOG_DEBUG("COLUMN_TEXT: PQgetvalue returned NULL, returning empty buffer");
                int buf = column_text_buf_idx;
                column_text_buf_idx = (column_text_buf_idx + 1) % NUM_TEXT_BUFFERS;
                column_text_buffers[buf][0] = '\0';
                pthread_mutex_unlock(&pg_stmt->mutex);
                return (const unsigned char*)column_text_buffers[buf];
            }
            
            // TYPE_DEBUG: Enhanced logging for type-related columns (column_text non-cached path)
            const char *col_name = PQfname(pg_stmt->result, idx);

            Oid oid = PQftype(pg_stmt->result, idx);
            
            // CRITICAL WARNING: column_text called for INTEGER column - this suggests SOCI type mismatch
            if (oid == 23 || oid == 20 || oid == 21) {  // int4, int8, int2
                LOG_DEBUG("COLUMN_TEXT_INTEGER: col='%s' idx=%d row=%d oid=%u val='%.50s' - INTEGER column accessed as TEXT!",
                          col_name ? col_name : "?", idx, row, oid, source_value);
                
                // TARGETED FIX: Only reformat aggregate function results (count, sum, max, min, avg)
                // These are the columns that cause std::bad_cast in SOCI
                if (col_name && (strcmp(col_name, "count") == 0 ||
                                strcmp(col_name, "sum") == 0 ||
                                strcmp(col_name, "max") == 0 ||
                                strcmp(col_name, "min") == 0 ||
                                strcmp(col_name, "avg") == 0 ||
                                strstr(col_name, "count(") != NULL ||
                                strstr(col_name, "COUNT(") != NULL)) {
                    // Reformat through sprintf to ensure clean string conversion
                    int buf_idx = column_text_buf_idx;
                    column_text_buf_idx = (column_text_buf_idx + 1) % NUM_TEXT_BUFFERS;
                    
                    if (oid == 20) {  // int8/BIGINT
                        long long val = atoll(source_value);
                        snprintf(column_text_buffers[buf_idx], TEXT_BUFFER_SIZE, "%lld", val);
                    } else {  // int2/int4
                        int val = atoi(source_value);
                        snprintf(column_text_buffers[buf_idx], TEXT_BUFFER_SIZE, "%d", val);
                    }
                    
                    LOG_DEBUG("COLUMN_TEXT_AGGREGATE_REFORMAT: col='%s' '%s' -> '%s'", 
                             col_name, source_value, column_text_buffers[buf_idx]);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return (const unsigned char*)column_text_buffers[buf_idx];
                }
            }
            
            if (col_name && strstr(col_name, "type") != NULL) {
                LOG_DEBUG("TYPE_DEBUG_TEXT: col='%s' idx=%d row=%d val='%.50s' sql=%.200s",
                          col_name, idx, row, source_value,
                          pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            }
        }

        // Copy strings to thread-local buffers instead of returning PQgetvalue() directly.
        // This ensures consistent behavior similar to native SQLite (alignment, ownership).
        
        // Validate UTF-8 first
        size_t str_len = strlen(source_value);
        if (str_len > 0 && !validate_utf8_string(source_value, str_len)) {
            LOG_ERROR("COLUMN_TEXT_UTF8_INVALID: idx=%d row=%d contains invalid UTF-8! len=%zu sql=%.200s",
                      idx, pg_stmt->current_row, str_len,
                      pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            // Return empty string for invalid UTF-8
            column_text_buffers[column_text_buf_idx][0] = '\0';
            const unsigned char *result = (const unsigned char*)column_text_buffers[column_text_buf_idx];
            column_text_buf_idx = (column_text_buf_idx + 1) % NUM_TEXT_BUFFERS;
            pthread_mutex_unlock(&pg_stmt->mutex);
            return result;
        }

        // Copy (or rewrite) into thread-local buffer
        int buf_idx = column_text_buf_idx;
        column_text_buf_idx = (column_text_buf_idx + 1) % NUM_TEXT_BUFFERS;

        if (rewrite_server_library_uri(source_value, column_text_buffers[buf_idx], TEXT_BUFFER_SIZE)) {
            LOG_DEBUG("COLUMN_TEXT_URI_REWRITE: idx=%d row=%d '%.80s' -> '%.80s'",
                      idx, pg_stmt->current_row, source_value, column_text_buffers[buf_idx]);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return (const unsigned char*)column_text_buffers[buf_idx];
        }

        size_t copy_len = (str_len < TEXT_BUFFER_SIZE - 1) ? str_len : (TEXT_BUFFER_SIZE - 1);
        memcpy(column_text_buffers[buf_idx], source_value, copy_len);
        column_text_buffers[buf_idx][copy_len] = '\0';
        
        LOG_DEBUG("COLUMN_TEXT: copied %zu bytes to buffer[%d] idx=%d row=%d utf8=valid",
                  copy_len, buf_idx, idx, pg_stmt->current_row);
        
        pthread_mutex_unlock(&pg_stmt->mutex);
        return (const unsigned char*)column_text_buffers[buf_idx];
    }
    LOG_DEBUG("COLUMN_TEXT: falling through to orig");
    return orig_sqlite3_column_text ? orig_sqlite3_column_text(pStmt, idx) : NULL;
}

const void* my_sqlite3_column_blob(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx] && crow->values[idx]) {
                    // Return cached blob data directly
                    // Note: For BYTEA, the cached value is already decoded
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return crow->values[idx];
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }

        if (idx < 0 || idx >= pg_stmt->num_cols || idx >= MAX_PARAMS) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            // Check if this is a BYTEA column (OID 17)
            Oid col_type = PQftype(pg_stmt->result, idx);
            const char *col_name = PQfname(pg_stmt->result, idx);
            LOG_DEBUG("column_blob called: col=%d name=%s type=%d row=%d", idx, col_name ? col_name : "?", col_type, row);
            if (col_type == 17) {  // BYTEA
                int blob_len;
                const void *result = pg_decode_bytea(pg_stmt, row, idx, &blob_len);
                pthread_mutex_unlock(&pg_stmt->mutex);
                return result;
            }

            // For non-BYTEA, cache the raw blob data to ensure pointer validity
            // Check if we already have this value cached for the current row
            if (pg_stmt->cached_row == row && pg_stmt->cached_blob[idx]) {
                const void *result = pg_stmt->cached_blob[idx];
                pthread_mutex_unlock(&pg_stmt->mutex);
                return result;
            }

            // Clear cache if row changed
            if (pg_stmt->cached_row != row) {
                for (int i = 0; i < MAX_PARAMS; i++) {
                    if (pg_stmt->cached_text[i]) {
                        free(pg_stmt->cached_text[i]);
                        pg_stmt->cached_text[i] = NULL;
                    }
                    if (pg_stmt->cached_blob[i]) {
                        free(pg_stmt->cached_blob[i]);
                        pg_stmt->cached_blob[i] = NULL;
                        pg_stmt->cached_blob_len[i] = 0;
                    }
                }
                pg_stmt->cached_row = row;
            }

            // Cache the blob data
            int blob_len = PQgetlength(pg_stmt->result, row, idx);
            const char *pg_value = PQgetvalue(pg_stmt->result, row, idx);
            if (pg_value && blob_len > 0) {
                pg_stmt->cached_blob[idx] = malloc(blob_len);
                if (pg_stmt->cached_blob[idx]) {
                    memcpy(pg_stmt->cached_blob[idx], pg_value, blob_len);
                    pg_stmt->cached_blob_len[idx] = blob_len;
                } else {
                    LOG_ERROR("COL_BLOB: malloc failed for column %d, len %d", idx, blob_len);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return NULL;
                }
            }
            const void *result = pg_stmt->cached_blob[idx];
            pthread_mutex_unlock(&pg_stmt->mutex);
            return result;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return NULL;
    }
    return orig_sqlite3_column_blob ? orig_sqlite3_column_blob(pStmt, idx) : NULL;
}

int my_sqlite3_column_bytes(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("COLUMN_BYTES: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // QUERY CACHE: Check for cached result first
        if (pg_stmt->cached_result) {
            cached_result_t *cached = pg_stmt->cached_result;
            int row = pg_stmt->current_row;
            if (idx >= 0 && idx < cached->num_cols && row >= 0 && row < cached->num_rows) {
                cached_row_t *crow = &cached->rows[row];
                if (!crow->is_null[idx]) {
                    int len = crow->lengths[idx];
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return len;
                }
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }

        if (idx < 0 || idx >= pg_stmt->num_cols) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        int row = pg_stmt->current_row;
        if (row < 0 || row >= pg_stmt->num_rows) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return 0;
        }
        if (!PQgetisnull(pg_stmt->result, row, idx)) {
            // Check if this is a BYTEA column (OID 17)
            Oid col_type = PQftype(pg_stmt->result, idx);
            if (col_type == 17) {  // BYTEA
                // Decode the blob (caches it) and return the decoded length
                int blob_len;
                pg_decode_bytea(pg_stmt, row, idx, &blob_len);
                pthread_mutex_unlock(&pg_stmt->mutex);
                return blob_len;
            }
            int len = PQgetlength(pg_stmt->result, row, idx);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return len;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return 0;
    }
    return orig_sqlite3_column_bytes ? orig_sqlite3_column_bytes(pStmt, idx) : 0;
}

const char* my_sqlite3_column_name(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("COLUMN_NAME: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // If no result yet but we have a query, execute it to get column metadata
        // SQLite allows column_name to be called before step()
        if (!pg_stmt->result && !pg_stmt->cached_result && pg_stmt->pg_sql) {
            if (!ensure_pg_result_for_metadata(pg_stmt)) {
                LOG_DEBUG("COLUMN_NAME: failed to execute query for metadata");
                pthread_mutex_unlock(&pg_stmt->mutex);
                return orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
            }
        }
        if (!pg_stmt->result) {
            LOG_DEBUG("COLUMN_NAME: pg_stmt has no result, falling back to orig");
            pthread_mutex_unlock(&pg_stmt->mutex);
            return orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
        }
        if (idx >= 0 && idx < pg_stmt->num_cols) {
            const char *name = PQfname(pg_stmt->result, idx);
            LOG_DEBUG("COLUMN_NAME: returning '%s' for idx=%d", name ? name : "NULL", idx);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return name;
        }
        LOG_DEBUG("COLUMN_NAME: idx out of bounds (num_cols=%d)", pg_stmt->num_cols);
        pthread_mutex_unlock(&pg_stmt->mutex);
    } else {
        LOG_DEBUG("COLUMN_NAME: not a PG stmt (pg_stmt=%p is_pg=%d), using orig",
                 (void*)pg_stmt, pg_stmt ? pg_stmt->is_pg : -1);
    }
    const char *orig_name = orig_sqlite3_column_name ? orig_sqlite3_column_name(pStmt, idx) : NULL;
    LOG_DEBUG("COLUMN_NAME: orig returned '%s'", orig_name ? orig_name : "NULL");
    return orig_name;
}

// sqlite3_column_decltype returns the declared type of a column from CREATE TABLE.
// CRITICAL FIX for std::bad_cast exceptions in SOCI:
// SOCI's SQLite3 backend uses a hardcoded type map (statement.cpp) to convert column values.
// When column_decltype returns NULL, SOCI defaults to "char" (db_string), but column_type
// returns SQLITE_INTEGER for booleans, causing a type mismatch that triggers std::bad_cast.
// Solution: Return the original SQLite declared type from metadata cache, with OID fallback.
// See: https://bugs.debian.org/cgi-bin/bugreport.cgi?bug=984534
const char* my_sqlite3_column_decltype(sqlite3_stmt *pStmt, int idx) {
    LOG_DEBUG("DECLTYPE_ENTRY: stmt=%p idx=%d", (void*)pStmt, idx);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    const int trace_any = trace_badcast_should_log(pg_stmt, idx);
    // CRITICAL DEBUG: Log all decltype calls
    LOG_DEBUG("DECLTYPE_CALLED: stmt=%p idx=%d pg_stmt=%p is_pg=%d",
             (void*)pStmt, idx, (void*)pg_stmt, pg_stmt ? pg_stmt->is_pg : -1);
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);

        // CRITICAL: If no result yet, execute query to get column metadata
        // SOCI calls column_decltype before step() to determine types
        if (!pg_stmt->result && !pg_stmt->cached_result && pg_stmt->pg_sql) {
            if (!ensure_pg_result_for_metadata(pg_stmt)) {
                LOG_ERROR("COLUMN_DECLTYPE: failed to execute query for metadata, returning TEXT");
                if (trace_any) {
                    LOG_INFO("TRACE_BADCAST: column_decltype idx=%d -> TEXT (metadata exec failed) sql=%.200s",
                             idx, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                return "TEXT";  // Safe fallback
            }
        }

        // Validate we have result and index is in range
        if (!pg_stmt->result || idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_DEBUG("DECLTYPE_NO_RESULT: result=%p idx=%d num_cols=%d, returning TEXT",
                     (void*)pg_stmt->result, idx, pg_stmt->num_cols);
            if (trace_any) {
                trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "noresult", pg_stmt->current_row, 0, 0, NULL);
                LOG_ERROR("TRACE_BADCAST: column_decltype idx=%d -> TEXT (no result / oob) sql=%.200s",
                         idx, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return "TEXT";  // Safe default that matches SQLITE_TEXT
        }

        const char *col_name = PQfname(pg_stmt->result, idx);
        const int trace = trace_badcast_should_log_col(pg_stmt, idx, col_name);
        const char *cached_type = NULL;
        
        // Debug logging for metadata_type specifically
        int is_metadata_type = (col_name && strstr(col_name, "metadata_type") != NULL);
        if (is_metadata_type) {
            LOG_DEBUG("DECLTYPE_DEBUG: START col='%s' idx=%d row=%d num_cols=%d",
                     col_name, idx, pg_stmt->current_row, pg_stmt->num_cols);
        }

        // STEP 1: Try looking up using column name as-is (for aliased columns like "devices_id")
        cached_type = lookup_sqlite_decltype(pg_stmt->conn, col_name);
        
        if (is_metadata_type) {
            LOG_DEBUG("DECLTYPE_DEBUG: STEP1 col='%s' cached_type='%s'",
                     col_name, cached_type ? cached_type : "(null)");
        }

        // STEP 2: If not found and we have a resolved table name, try table_column format
        if (!cached_type && idx < MAX_PARAMS && pg_stmt->col_table_names[idx]) {
            // Column name is bare (e.g., "extra_data"), construct "table_column" key
            char cache_key[DECLTYPE_MAX_KEY_LEN];
            snprintf(cache_key, sizeof(cache_key), "%s_%s",
                     pg_stmt->col_table_names[idx], col_name);
            cached_type = lookup_decltype_direct(pg_stmt->conn, cache_key);
            if (cached_type) {
                LOG_INFO("DECLTYPE_RESOLVED: bare col '%s' -> table '%s' -> '%s'",
                         col_name, pg_stmt->col_table_names[idx], cached_type);
            }
            if (is_metadata_type) {
                LOG_DEBUG("DECLTYPE_DEBUG: STEP2 table='%s' cache_key='%s' cached_type='%s'",
                         pg_stmt->col_table_names[idx], cache_key, cached_type ? cached_type : "(null)");
            }
        } else if (is_metadata_type) {
            LOG_DEBUG("DECLTYPE_DEBUG: STEP2 SKIPPED (cached_type=%s idx=%d has_table=%d)",
                     cached_type ? cached_type : "(null)", idx,
                     (idx < MAX_PARAMS && pg_stmt->col_table_names[idx]) ? 1 : 0);
        }

        // STEP 3: If found in cache, return the original SQLite declared type
        if (cached_type) {
            if (is_metadata_type) {
                LOG_DEBUG("DECLTYPE_DEBUG: RETURNING CACHED='%s' for col='%s' idx=%d",
                         cached_type, col_name, idx);
            }
            // CRITICAL DEBUG: Log cached decltype returns too
            LOG_DEBUG("DECLTYPE_CACHED: idx=%d col='%s' -> '%s' sql=%.300s",
                     idx, col_name ? col_name : "?", cached_type,
                     pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            if (trace) {
                Oid oid = PQftype(pg_stmt->result, idx);
                trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "cached", pg_stmt->current_row, 0, oid, col_name);
                LOG_ERROR("TRACE_BADCAST: column_decltype (cached) idx=%d col='%s' oid=%u -> '%s' sql=%.200s",
                         idx, col_name ? col_name : "?", (unsigned)oid,
                         cached_type, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return cached_type;
        }

        // STEP 4: Fallback to OID-based type mapping
        Oid oid = PQftype(pg_stmt->result, idx);

        // Plex uses a custom declared type for datetime columns in its SQLite schema:
        //   dt_integer(8)
        // When we emulate SQLite against PostgreSQL, preserving this decltype for
        // timestamp-ish BIGINT columns avoids downstream type assumptions that can
        // lead to std::bad_cast in Plex's DB layer.
        if (oid == 20 && col_name) {
            // Common Plex timestamp columns.
            if (strstr(col_name, "_at") != NULL ||
                strstr(col_name, "timestamp") != NULL ||
                strstr(col_name, "time") != NULL) {
                if (trace) {
                    trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "dt_integer(8)", pg_stmt->current_row, 0, oid, col_name);
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                return "dt_integer(8)";
            }

            // Special-case the metadata_items refresh query:
            //   select GREATEST(max(metadata_items.changed_at), max(metadata_items.resources_changed_at)) ...
            // The result is a Plex datetime int8.
            if (strcmp(col_name, "greatest") == 0 && pg_stmt->pg_sql &&
                strstr(pg_stmt->pg_sql, "metadata_items.changed_at") != NULL) {
                if (trace) {
                    trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "dt_integer(8)", pg_stmt->current_row, 0, oid, col_name);
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                return "dt_integer(8)";
            }
        }
        
        // SPECIAL CASE: Aggregate functions (count, sum, max, min, avg) 
        // PostgreSQL returns BIGINT (OID 20) for aggregates
        // Different Plex call sites expect different widths. In practice:
        // - count(*) is often consumed as a 32-bit INTEGER
        // - other aggregates (sum/max/min/avg) may require 64-bit handling
        if (col_name && oid == 20) {
            const int is_countish =
                strcmp(col_name, "count") == 0 ||
                strcmp(col_name, "cnt") == 0 ||
                strstr(col_name, "count(") != NULL ||
                strstr(col_name, "COUNT(") != NULL ||
                (pg_stmt->pg_sql && (strstr(pg_stmt->pg_sql, "count(") != NULL || strstr(pg_stmt->pg_sql, "COUNT(") != NULL));

            if (is_countish) {
                const int has_table = (idx < MAX_PARAMS && pg_stmt->col_table_names[idx]);
                // Native SQLite returns NULL decltype for computed expressions like count(*).
                // Matching that behavior avoids SOCI/Plex making incorrect type assumptions.
                if (!has_table) {
                    if (trace) {
                        trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "NULL", pg_stmt->current_row, 0, oid, col_name);
                    }
                    LOG_DEBUG("DECLTYPE_AGGREGATE: col='%s' OID=20 (count expr) -> returning NULL", col_name);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return NULL;
                }
                if (trace) {
                    trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "INTEGER", pg_stmt->current_row, 0, oid, col_name);
                }
                LOG_DEBUG("DECLTYPE_AGGREGATE: col='%s' OID=20 (BIGINT/count) -> returning INTEGER", col_name);
                pthread_mutex_unlock(&pg_stmt->mutex);
                return "INTEGER";
            }

            // For other aggregates returning int8, keep Plex's 8-byte token.
            if (strcmp(col_name, "sum") == 0 ||
                strcmp(col_name, "max") == 0 ||
                strcmp(col_name, "min") == 0 ||
                strcmp(col_name, "avg") == 0) {
                const int has_table = (idx < MAX_PARAMS && pg_stmt->col_table_names[idx]);
                if (!has_table) {
                    if (trace) {
                        trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "NULL", pg_stmt->current_row, 0, oid, col_name);
                    }
                    LOG_DEBUG("DECLTYPE_AGGREGATE: col='%s' OID=20 (agg expr) -> returning NULL", col_name);
                    pthread_mutex_unlock(&pg_stmt->mutex);
                    return NULL;
                }
                if (trace) {
                    trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "dt_integer(8)", pg_stmt->current_row, 0, oid, col_name);
                }
                LOG_DEBUG("DECLTYPE_AGGREGATE: col='%s' OID=20 (BIGINT/agg) -> returning dt_integer(8)", col_name);
                pthread_mutex_unlock(&pg_stmt->mutex);
                return "dt_integer(8)";
            }
        }
        
        // Use centralized OID-to-decltype mapping function
        // CRITICAL: This function now differentiates INT4 (OID 23) -> "INTEGER" 
        //           from INT8 (OID 20) -> "BIGINT" to prevent std::bad_cast
        const char *decltype = pg_oid_to_sqlite_decltype(oid);

        if (trace) {
            trace_badcast_log_ctx(pg_stmt, pStmt, idx, "column_decltype", "oid", pg_stmt->current_row, 0, oid, col_name);
            LOG_ERROR("TRACE_BADCAST: column_decltype idx=%d col='%s' oid=%u -> '%s' sql=%.200s",
                     idx, col_name ? col_name : "?", (unsigned)oid,
                     decltype ? decltype : "(null)", pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
        }

        // LOG INT8 vs INT4 detection (for debugging type issues)
        if (strcmp(decltype, "BIGINT") == 0 || strcmp(decltype, "INTEGER") == 0) {
            LOG_DEBUG("DECLTYPE_INT: col='%s' idx=%d oid=%u -> '%s' sql=%.100s",
                      col_name ? col_name : "?", idx, (unsigned)oid, decltype,
                      pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
        }
        
        LOG_DEBUG("DECLTYPE_CACHED: idx=%d col='%s' -> '%s' sql=%.100s",
                 idx, col_name ? col_name : "?", decltype,
                 pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
        
        pthread_mutex_unlock(&pg_stmt->mutex);
        return decltype;
    }
    LOG_DEBUG("DECLTYPE_FALLBACK: using orig (pg_stmt=%p is_pg=%d)",
             (void*)pg_stmt, pg_stmt ? pg_stmt->is_pg : -1);
    const char *orig_type = orig_sqlite3_column_decltype ? orig_sqlite3_column_decltype(pStmt, idx) : NULL;
    LOG_DEBUG("COLUMN_DECLTYPE: orig returned '%s'", orig_type ? orig_type : "NULL");
    return orig_type;
}
// sqlite3_column_value returns a pointer to a sqlite3_value for a column.
// For PostgreSQL statements, we return a fake sqlite3_value that encodes the pg_stmt and column.
// The sqlite3_value_* functions will decode this to return proper PostgreSQL data.
sqlite3_value* my_sqlite3_column_value(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // column_value is typically called after step(), but just in case...
        if (!pg_stmt->result && !pg_stmt->cached_result && pg_stmt->pg_sql) {
            if (!ensure_pg_result_for_metadata(pg_stmt)) {
                LOG_DEBUG("COLUMN_VALUE: failed to execute query for metadata");
                pthread_mutex_unlock(&pg_stmt->mutex);
                return orig_sqlite3_column_value ? orig_sqlite3_column_value(pStmt, idx) : NULL;
            }
        }
        if (!pg_stmt->result) {
            pthread_mutex_unlock(&pg_stmt->mutex);
            return orig_sqlite3_column_value ? orig_sqlite3_column_value(pStmt, idx) : NULL;
        }
        if (idx < 0 || idx >= pg_stmt->num_cols) {
            LOG_DEBUG("COLUMN_VALUE_BOUNDS: idx=%d out of bounds (num_cols=%d) sql=%.100s",
                     idx, pg_stmt->num_cols, pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            pthread_mutex_unlock(&pg_stmt->mutex);
            return NULL;
        }
        int row = pg_stmt->current_row;
        pthread_mutex_unlock(&pg_stmt->mutex);

        // Return a fake value from our pool (thread-safe)
        pthread_mutex_lock(&fake_value_mutex);
        // Use bitmask instead of modulo - always produces 0-255 even after overflow
        unsigned int slot = (fake_value_next++) & 0xFF;
        pg_fake_value_t *fake = &fake_value_pool[slot];
        fake->magic = PG_FAKE_VALUE_MAGIC;
        fake->pg_stmt = pg_stmt;
        fake->col_idx = idx;
        fake->row_idx = row;
        pthread_mutex_unlock(&fake_value_mutex);

        return (sqlite3_value*)fake;
    }
    return orig_sqlite3_column_value ? orig_sqlite3_column_value(pStmt, idx) : NULL;
}

int my_sqlite3_data_count(sqlite3_stmt *pStmt) {
    LOG_DEBUG("DATA_COUNT: stmt=%p", (void*)pStmt);
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    // Handle all PostgreSQL statements
    if (pg_stmt && pg_stmt->is_pg) {
        pthread_mutex_lock(&pg_stmt->mutex);
        // For PostgreSQL statements, return our stored num_cols if we have a valid row
        // Don't fall through to orig_sqlite3_data_count which would fail
        int count = (pg_stmt->current_row < pg_stmt->num_rows) ? pg_stmt->num_cols : 0;
        pthread_mutex_unlock(&pg_stmt->mutex);
        LOG_DEBUG("DATA_COUNT: returning %d (row=%d rows=%d cols=%d)",
                 count, pg_stmt->current_row, pg_stmt->num_rows, pg_stmt->num_cols);
        return count;
    }
    return orig_sqlite3_data_count ? orig_sqlite3_data_count(pStmt) : 0;
}

// Value functions (my_sqlite3_value_*) are in db_interpose_value.c
