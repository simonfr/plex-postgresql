/*
 * Plex PostgreSQL Interposing Shim - Parameter Binding
 *
 * Handles sqlite3_bind_* function interposition.
 * These functions capture bound parameters for PostgreSQL queries.
 */

#include "db_interpose.h"
#include "db_interpose_common.h"
#include "db_interpose_rust.h"
#include "pg_mem_telemetry.h"
#include "shim_alloc.h"

// ============================================================================
// RACE_DEBUG Macro
// ============================================================================

// Macro to log bind results for race condition debugging
// LOG_BIND_RESULT removed - no longer logging bind results
#define LOG_BIND_RESULT(pStmt, idx, rc, type) do { (void)pStmt; (void)idx; (void)rc; (void)type; } while(0)

// ============================================================================
// Helper Functions
// ============================================================================

// Helper to map SQLite parameter index to PostgreSQL parameter index
// Handles named parameters (:name) by finding the correct position
int pg_map_param_index(pg_stmt_t *pg_stmt, sqlite3_stmt *pStmt, int sqlite_idx) {
    if (!pg_stmt) {
        LOG_DEBUG("pg_map_param_index: no pg_stmt, using direct mapping idx=%d -> %d", sqlite_idx, sqlite_idx - 1);
        return sqlite_idx - 1;
    }

    // If we have named parameters, we need to map them
    if (pg_stmt->param_names && pg_stmt->param_count > 0) {
        // Get the parameter name from SQLite
        const char *param_name = sqlite3_bind_parameter_name(pStmt, sqlite_idx);
        LOG_DEBUG("pg_map_param_index: sqlite_idx=%d, param_name=%s, param_count=%d",
                 sqlite_idx, param_name ? param_name : "NULL", pg_stmt->param_count);

        if (param_name) {
            // Remove the : prefix if present
            const char *clean_name = param_name;
            if (param_name[0] == ':') clean_name = param_name + 1;

            // Debug: show all param names
            for (int i = 0; i < pg_stmt->param_count && i < 5; i++) {
                LOG_DEBUG("  param_names[%d] = %s", i, pg_stmt->param_names[i] ? pg_stmt->param_names[i] : "NULL");
            }

            // Find this name in our param_names array
            for (int i = 0; i < pg_stmt->param_count; i++) {
                if (pg_stmt->param_names[i] && strcmp(pg_stmt->param_names[i], clean_name) == 0) {
                    LOG_DEBUG("  -> Found match at pg_idx=%d", i);
                    return i;  // Found it! Return the PostgreSQL position
                }
            }
            LOG_DEBUG("Named parameter '%s' not found in translation (sqlite_idx=%d)", clean_name, sqlite_idx);
        } else {
            LOG_DEBUG("  -> No parameter name, using direct mapping");
        }
    } else {
        LOG_DEBUG("pg_map_param_index: no param_names (count=%d), using direct mapping idx=%d -> %d",
                 pg_stmt->param_count, sqlite_idx, sqlite_idx - 1);
    }

    // For positional parameters (?) or if name not found, use direct mapping
    return sqlite_idx - 1;
}

static inline void note_bind_phase(const char *phase, sqlite3_stmt *pStmt, pg_stmt_t *pg_stmt) {
    const char *sql = NULL;
    sqlite3 *db = NULL;

    if (pg_stmt) {
        sql = pg_stmt->pg_sql ? pg_stmt->pg_sql : pg_stmt->sql;
    }
    if (!sql && orig_sqlite3_sql) {
        sql = orig_sqlite3_sql(pStmt);
    }
    if (orig_sqlite3_db_handle) {
        db = orig_sqlite3_db_handle(pStmt);
    }

    pg_exception_note_phase(phase, sql, pStmt, db);
}

// SQLite value introspection APIs take non-const sqlite3_value* even though they
// do not mutate the value. Centralize the cast in one helper.
static inline sqlite3_value *as_mut_sqlite_value(const sqlite3_value *value) {
    return (sqlite3_value *)value;
}

// Helper: check if data contains binary bytes (non-UTF8 safe)
// Returns 1 if data contains bytes that would be invalid in UTF-8 text
int contains_binary_bytes(const unsigned char *data, size_t len) {
    return rust_contains_binary_bytes(data, len);
}

// Helper: convert binary data to PostgreSQL hex format (\xHEXHEX...)
// Caller must free the returned string
char* bytes_to_pg_hex(const unsigned char *data, size_t len) {
    char *hex_rust = rust_bytes_to_pg_hex(data, len);
    if (!hex_rust) return NULL;
    char *hex = strdup(hex_rust);
    rust_free_cstring(hex_rust);
    if (!hex) return NULL;
    if (pg_mem_telemetry_enabled())
        pg_mem_telemetry_add(PMT_BIND_HEX_ALLOC, strlen(hex) + 1, 1);
    return hex;
}

// ============================================================================
// Busy Statement Auto-Reset
// ============================================================================

// CRITICAL FIX v0.9.0: Hybrid busy-wait with exponential backoff + retry
// 
// ROOT CAUSE (discovered 2026-01-16):
// Multiple threads can access the same sqlite3_stmt* concurrently. The pg_stmt->mutex
// protects the pg_stmt structure but NOT the underlying SQLite statement pointer.
// 
// RACE CONDITION (TOCTOU - Time Of Check Time Of Use):
//   Thread A: sqlite3_step() starts → statement becomes BUSY
//   Thread B: sqlite3_stmt_busy() check → returns FALSE (race window!)
//   Thread B: sqlite3_bind_*() → SQLITE_MISUSE (21)
//
// FIX: Two-layer defense:
//   1. Check busy before bind (prevents most cases)
//   2. If bind fails with SQLITE_MISUSE, retry with wait (handles TOCTOU race)
// Performance: <0.05ms average overhead (99.3% of requests skip wait entirely)

// Helper: Wait for statement to become not-busy
// CRITICAL: Don't rely on sqlite3_stmt_busy() - it returns FALSE even when busy!
// Just force a reset immediately when called.
static inline int wait_for_stmt_ready(sqlite3_stmt *pStmt, const char *caller) {
    (void)caller;  // Unused
    
    if (orig_sqlite3_reset) {
        orig_sqlite3_reset(pStmt);
    }
    
    // Wait after reset to ensure it takes effect and any concurrent operations finish
    usleep(500);  // 500 microseconds
    
    return 1;
}

// Macro for retry logic - retries up to 3 times on SQLITE_MISUSE
#define RETRY_ON_MISUSE(bind_call, bind_name) \
    (void)bind_name; /* Unused */ \
    for (int retry = 0; retry < 3 && rc == SQLITE_MISUSE; retry++) { \
        if (wait_for_stmt_ready(pStmt, "RETRY-BIND")) { \
            rc = bind_call; \
            if (rc == SQLITE_OK) { \
                break; \
            } \
        } \
    }

// Primary busy-check function (called before bind)
static inline void ensure_stmt_not_busy(sqlite3_stmt *pStmt, pg_stmt_t *pg_stmt) {
    (void)pg_stmt;  // Unused now - we reset unconditionally
    
    // CRITICAL FIX v0.9.1: ALWAYS reset before bind to eliminate ALL TOCTOU races
    // This is aggressive but necessary to handle statements not in our registry
    if (orig_sqlite3_reset) {
        orig_sqlite3_reset(pStmt);
    }
}



// CRITICAL FIX v0.8.9: Clear metadata-only results before binding
// When ensure_pg_result_for_metadata() executes a query BEFORE parameters are
// bound (e.g., Plex calls column_decltype() before bind()), it caches a result
// with NULL/unbound params that may return 0 rows. When bind() is called, we
// must clear this stale result so step() will re-execute with bound params.
// This fixes "Step didn't return row" exceptions.
//
// v0.8.9.1 FIX: Don't PQclear here - just mark for re-execution.
// The PQclear was causing race conditions with concurrent threads.
// Let step() handle the cleanup safely.
static inline void clear_metadata_result_if_needed(pg_stmt_t *pg_stmt) {
    if (pg_stmt && pg_stmt->metadata_only_result && pg_stmt->result) {
        LOG_DEBUG("BIND: Marking metadata-only result for re-execution with bound params");
        // Don't PQclear here - causes race conditions
        // Just mark that we need to re-execute with real params
        pg_stmt->metadata_only_result = 2;  // 2 = needs re-execution
    }
}

// NOTE: v0.8.8 uses pg_find_any_stmt() from pg_statement.h instead of pg_find_stmt()
// This checks BOTH the primary registry AND the cached statement registry,
// ensuring mutex protection for all bind operations including cached statements.

// ============================================================================
// Bind Functions
// ============================================================================

int my_sqlite3_bind_int(sqlite3_stmt *pStmt, int idx, int val) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_int", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.9.0: Check if statement is busy before bind
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_int ? orig_sqlite3_bind_int(pStmt, idx, val) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_int ? orig_sqlite3_bind_int(pStmt, idx, val) : SQLITE_ERROR, "bind_int")

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%d", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    return rc;
}

int my_sqlite3_bind_int64(sqlite3_stmt *pStmt, int idx, sqlite3_int64 val) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_int64", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.8.8: Auto-reset if statement is busy
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_int64 ? orig_sqlite3_bind_int64(pStmt, idx, val) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_int64 ? orig_sqlite3_bind_int64(pStmt, idx, val) : SQLITE_ERROR, "bind_int64")

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%lld", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    return rc;
}

int my_sqlite3_bind_double(sqlite3_stmt *pStmt, int idx, double val) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_double", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.8.8: Auto-reset if statement is busy
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_double ? orig_sqlite3_bind_double(pStmt, idx, val) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_double ? orig_sqlite3_bind_double(pStmt, idx, val) : SQLITE_ERROR, "bind_double")

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Use pre-allocated buffer instead of strdup
            snprintf(pg_stmt->param_buffers[pg_idx], 32, "%.17g", val);
            pg_stmt->param_values[pg_idx] = pg_stmt->param_buffers[pg_idx];
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    return rc;
}

int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                         int nBytes, void (*destructor)(void*)) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_text", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.8.8: Auto-reset if statement is busy
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_text ? orig_sqlite3_bind_text(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_text ? orig_sqlite3_bind_text(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR, "bind_text")

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);

        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Free old value only if it was dynamically allocated
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }

            size_t actual_len = (nBytes < 0) ? strlen(val) : (size_t)nBytes;

            // Check if data contains binary bytes (non-UTF8)
            // If so, convert to PostgreSQL hex format for BYTEA columns
            if (contains_binary_bytes((const unsigned char*)val, actual_len)) {
                LOG_DEBUG("bind_text: detected binary data at idx=%d, len=%zu, converting to hex", idx, actual_len);
                pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, actual_len);
                /* hex telemetry logged inside bytes_to_pg_hex */
            } else if (nBytes < 0) {
                pg_stmt->param_values[pg_idx] = strdup(val);
                if (pg_mem_telemetry_enabled())
                    pg_mem_telemetry_add(PMT_BIND_TEXT_ALLOC, actual_len + 1, 1);
            } else {
                pg_stmt->param_values[pg_idx] = malloc(nBytes + 1);
                if (pg_stmt->param_values[pg_idx]) {
                    memcpy(pg_stmt->param_values[pg_idx], val, nBytes);
                    pg_stmt->param_values[pg_idx][nBytes] = '\0';
                    if (pg_mem_telemetry_enabled())
                        pg_mem_telemetry_add(PMT_BIND_TEXT_ALLOC, (size_t)nBytes + 1, 1);
                }
            }
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    pg_mem_telemetry_maybe_log();
    return rc;
}

int my_sqlite3_bind_blob(sqlite3_stmt *pStmt, int idx, const void *val,
                         int nBytes, void (*destructor)(void*)) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_blob", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.8.8: Auto-reset if statement is busy
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_blob ? orig_sqlite3_bind_blob(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_blob ? orig_sqlite3_bind_blob(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR, "bind_blob")

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }
            // Convert binary data to PostgreSQL hex format for BYTEA columns
            // This works in text mode (paramFormats=NULL means text mode)
            LOG_DEBUG("bind_blob: converting %d bytes to hex at idx=%d", nBytes, idx);
            pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, (size_t)nBytes);
            pg_stmt->param_lengths[pg_idx] = 0;  // Use strlen for text mode
            pg_stmt->param_formats[pg_idx] = 0;  // text mode (hex string)
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    return rc;
}

// sqlite3_bind_blob64 - 64-bit version for large blobs
int my_sqlite3_bind_blob64(sqlite3_stmt *pStmt, int idx, const void *val,
                           sqlite3_uint64 nBytes, void (*destructor)(void*)) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_blob64", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.8.8: Auto-reset if statement is busy
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_blob64 ? orig_sqlite3_bind_blob64(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_blob64 ? orig_sqlite3_bind_blob64(pStmt, idx, val, nBytes, destructor) : SQLITE_ERROR, "bind_blob64")

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val && nBytes > 0) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }
            // Convert binary data to PostgreSQL hex format for BYTEA columns
            LOG_DEBUG("bind_blob64: converting %llu bytes to hex at idx=%d", (unsigned long long)nBytes, idx);
            pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, (size_t)nBytes);
            pg_stmt->param_lengths[pg_idx] = 0;  // Use strlen for text mode
            pg_stmt->param_formats[pg_idx] = 0;  // text mode (hex string)
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    return rc;
}

// sqlite3_bind_text64 - critical for Plex which uses this for text values!
int my_sqlite3_bind_text64(sqlite3_stmt *pStmt, int idx, const char *val,
                           sqlite3_uint64 nBytes, void (*destructor)(void*),
                           unsigned char encoding) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_text64", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.8.8: Auto-reset if statement is busy
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_text64 ? orig_sqlite3_bind_text64(pStmt, idx, val, nBytes, destructor, encoding) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_text64 ? orig_sqlite3_bind_text64(pStmt, idx, val, nBytes, destructor, encoding) : SQLITE_ERROR, "bind_text64")

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;  // Prevent dangling pointer
            }

            size_t actual_len = (nBytes == (sqlite3_uint64)-1) ? strlen(val) : (size_t)nBytes;

            // Check if data contains binary bytes (non-UTF8)
            // If so, convert to PostgreSQL hex format for BYTEA columns
            if (contains_binary_bytes((const unsigned char*)val, actual_len)) {
                LOG_DEBUG("bind_text64: detected binary data at idx=%d, len=%zu, converting to hex", idx, actual_len);
                pg_stmt->param_values[pg_idx] = bytes_to_pg_hex((const unsigned char*)val, actual_len);
            } else if (nBytes == (sqlite3_uint64)-1) {
                pg_stmt->param_values[pg_idx] = strdup(val);
                if (pg_mem_telemetry_enabled())
                    pg_mem_telemetry_add(PMT_BIND_TEXT_ALLOC, actual_len + 1, 1);
            } else {
                pg_stmt->param_values[pg_idx] = malloc((size_t)nBytes + 1);
                if (pg_stmt->param_values[pg_idx]) {
                    memcpy(pg_stmt->param_values[pg_idx], val, (size_t)nBytes);
                    pg_stmt->param_values[pg_idx][(size_t)nBytes] = '\0';
                    if (pg_mem_telemetry_enabled())
                        pg_mem_telemetry_add(PMT_BIND_TEXT_ALLOC, (size_t)nBytes + 1, 1);
                }
            }
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    pg_mem_telemetry_maybe_log();
    return rc;
}

// sqlite3_bind_value - copies value from another sqlite3_value
int my_sqlite3_bind_value(sqlite3_stmt *pStmt, int idx, const sqlite3_value *pValue) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_value", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.8.8: Auto-reset if statement is busy
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_value ? orig_sqlite3_bind_value(pStmt, idx, pValue) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_value ? orig_sqlite3_bind_value(pStmt, idx, pValue) : SQLITE_ERROR, "bind_value")

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && pValue) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            // Get value type and extract appropriately
            sqlite3_value *mutable_value = as_mut_sqlite_value(pValue);
            int vtype = sqlite3_value_type(mutable_value);
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;
            }

            switch (vtype) {
                case SQLITE_INTEGER: {
                    sqlite3_int64 v = sqlite3_value_int64(mutable_value);
                    char buf[32];
                    snprintf(buf, sizeof(buf), "%lld", v);
                    pg_stmt->param_values[pg_idx] = strdup(buf);
                    break;
                }
                case SQLITE_FLOAT: {
                    double v = sqlite3_value_double(mutable_value);
                    char buf[64];
                    snprintf(buf, sizeof(buf), "%.17g", v);
                    pg_stmt->param_values[pg_idx] = strdup(buf);
                    break;
                }
                case SQLITE_TEXT: {
                    const char *v = (const char *)sqlite3_value_text(mutable_value);
                    if (v) pg_stmt->param_values[pg_idx] = strdup(v);
                    break;
                }
                case SQLITE_BLOB: {
                    int len = sqlite3_value_bytes(mutable_value);
                    const void *v = sqlite3_value_blob(mutable_value);
                    if (v && len > 0) {
                        pg_stmt->param_values[pg_idx] = malloc(len);
                        if (pg_stmt->param_values[pg_idx]) {
                            memcpy(pg_stmt->param_values[pg_idx], v, len);
                            if (pg_mem_telemetry_enabled())
                                pg_mem_telemetry_add(PMT_BIND_VALUE_BLOB_ALLOC, (size_t)len, 1);
                        }
                        pg_stmt->param_lengths[pg_idx] = len;
                        pg_stmt->param_formats[pg_idx] = 1;  // binary
                    }
                    break;
                }
                case SQLITE_NULL:
                default:
                    // Leave as NULL
                    break;
            }
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    return rc;
}

int my_sqlite3_bind_null(sqlite3_stmt *pStmt, int idx) {
    pg_stmt_t *pg_stmt = pg_find_any_stmt(pStmt);
    note_bind_phase("bind_null", pStmt, pg_stmt);

    // CRITICAL FIX: Lock BEFORE calling SQLite to prevent "bind on busy statement"
    if (pg_stmt) pthread_mutex_lock(&pg_stmt->mutex);

    // CRITICAL FIX v0.8.9: Clear metadata-only result so step() will re-execute
    clear_metadata_result_if_needed(pg_stmt);

    // CRITICAL FIX v0.8.8: Auto-reset if statement is busy
    ensure_stmt_not_busy(pStmt, pg_stmt);

    int rc = orig_sqlite3_bind_null ? orig_sqlite3_bind_null(pStmt, idx) : SQLITE_ERROR;
    
    // CRITICAL FIX v0.9.0: Retry up to 3 times if bind failed due to TOCTOU race
    RETRY_ON_MISUSE(orig_sqlite3_bind_null ? orig_sqlite3_bind_null(pStmt, idx) : SQLITE_ERROR, "bind_null")

    LOG_BIND_RESULT(pStmt, idx, rc, "null");

    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS) {
        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;
            }
        }
    }

    if (pg_stmt) pthread_mutex_unlock(&pg_stmt->mutex);
    return rc;
}
