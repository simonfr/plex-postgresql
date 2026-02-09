/*
 * Plex PostgreSQL Interposing Shim - Value Access
 *
 * Handles sqlite3_value_* function interposition.
 * These functions read data from fake sqlite3_value pointers that encode
 * a pg_stmt_t reference + column/row indices, produced by my_sqlite3_column_value
 * in db_interpose_column.c.
 */

#include "db_interpose.h"
#include <stdatomic.h>

// Helper to convert SQLite type constant to string for logging
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

// ============================================================================
// Value Functions (for sqlite3_column_value returned values)
// ============================================================================

// Counter for value function calls (for debugging)
static atomic_long value_type_calls = 0;
static atomic_long value_text_calls = 0;
static atomic_long value_int_calls = 0;

// Intercept sqlite3_value_type to handle our fake values
// CRITICAL: Must hold mutex while accessing pg_stmt->result to prevent race conditions
int my_sqlite3_value_type(sqlite3_value *pVal) {
    global_value_type_calls++;  // Global counter for exception debugging
    if (!pVal) return SQLITE_NULL;  // CRITICAL FIX: NULL check to prevent crash
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        long call_num = atomic_fetch_add(&value_type_calls, 1);

        // Update context for exception debugging (TLS)
        last_query_being_processed = pg_stmt->pg_sql;

        // CRITICAL FIX: Lock mutex before accessing result to prevent use-after-free
        pthread_mutex_lock(&pg_stmt->mutex);
        
        if (pg_stmt->result && fake->row_idx >= 0 && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            int is_null = PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx);
            Oid oid = PQftype(pg_stmt->result, fake->col_idx);
            const char *col_name = PQfname(pg_stmt->result, fake->col_idx);

            // Update context
            last_column_being_accessed = col_name;

            int result;
            if (is_null) {
                result = SQLITE_NULL;
            } else {
                switch (oid) {
                    case 20: case 21: case 23: case 26: case 16:  // int8, int2, int4, oid, bool
                        result = SQLITE_INTEGER;
                        break;
                    case 700: case 701: case 1700:  // float4, float8, numeric
                        result = SQLITE_FLOAT;
                        break;
                    case 17:  // bytea
                        result = SQLITE_BLOB;
                        break;
                    default:
                        result = SQLITE_TEXT;
                        break;
                }
            }

            // Log every 1000th call to reduce overhead (was 100)
            if (call_num % 1000 == 0) {
                LOG_INFO("VALUE_TYPE[%ld]: col='%s' row=%d OID=%u is_null=%d -> %s sql=%.60s",
                        call_num, col_name ? col_name : "?", fake->row_idx,
                        (unsigned)oid, is_null, sqlite_type_name(result),
                        pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            }
            pthread_mutex_unlock(&pg_stmt->mutex);
            return result;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        LOG_INFO("VALUE_TYPE[%ld]: FAKE VALUE but no result (row=%d col=%d)",
                call_num, fake->row_idx, fake->col_idx);
        return SQLITE_NULL;
    }
    return orig_sqlite3_value_type ? orig_sqlite3_value_type(pVal) : SQLITE_NULL;
}

// Intercept sqlite3_value_text to handle our fake values
// Static buffers for value_text - LARGE pool, separate from column_text
// 256 buffers x 16KB = 4MB total - prevents race condition wrap-around
static char value_text_buffers[256][16384];
static atomic_int value_text_idx = 0;  // Atomic for thread-safe increment

const unsigned char* my_sqlite3_value_text(sqlite3_value *pVal) {
    if (!pVal) return NULL;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        long call_num = atomic_fetch_add(&value_text_calls, 1);
        pthread_mutex_lock(&pg_stmt->mutex);
        if (pg_stmt->result && fake->row_idx >= 0 && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                if (call_num % 100 == 0) {
                    LOG_INFO("VALUE_TEXT[%ld]: col=%d row=%d -> NULL (is_null)", call_num, fake->col_idx, fake->row_idx);
                }
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }
            // CRITICAL FIX: Copy to static buffer instead of returning PGresult pointer directly
            // This prevents use-after-free when PGresult is cleared
            const char* pg_value = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!pg_value) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }

            size_t len = strlen(pg_value);
            if (len >= 16383) len = 16383;

            // Thread-safe buffer allocation using atomic increment
            int buf = atomic_fetch_add(&value_text_idx, 1) & 0xFF;
            memcpy(value_text_buffers[buf], pg_value, len);
            value_text_buffers[buf][len] = '\0';

            // Log every 100th call with value preview
            if (call_num % 100 == 0) {
                const char *col_name = PQfname(pg_stmt->result, fake->col_idx);
                LOG_INFO("VALUE_TEXT[%ld]: col='%s' row=%d val='%.30s%s'",
                        call_num, col_name ? col_name : "?", fake->row_idx,
                        value_text_buffers[buf], len > 30 ? "..." : "");
            }

            pthread_mutex_unlock(&pg_stmt->mutex);
            return (const unsigned char*)value_text_buffers[buf];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return NULL;
    }
    return orig_sqlite3_value_text ? orig_sqlite3_value_text(pVal) : NULL;
}

// Intercept sqlite3_value_int to handle our fake values
// CRITICAL: Must hold mutex while accessing pg_stmt->result
int my_sqlite3_value_int(sqlite3_value *pVal) {
    if (!pVal) return 0;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        long call_num = atomic_fetch_add(&value_int_calls, 1);
        (void)call_num;  // Suppress unused warning
        
        pthread_mutex_lock(&pg_stmt->mutex);
        if (pg_stmt->result && fake->row_idx >= 0 && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return 0;
            }
            const char *val = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!val) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return 0;
            }
            int result;
            // Handle PostgreSQL boolean 't'/'f' format
            if (val[0] == 't' && val[1] == '\0') result = 1;
            else if (val[0] == 'f' && val[1] == '\0') result = 0;
            else result = atoi(val);

            // TYPE_DEBUG: Enhanced logging for type-related columns (value_int path)
            const char *col_name = PQfname(pg_stmt->result, fake->col_idx);
            if (col_name && strstr(col_name, "type") != NULL) {
                LOG_DEBUG("TYPE_DEBUG_VALUE_INT: col='%s' idx=%d row=%d raw_val='%s' result=%d sql=%.200s",
                          col_name, fake->col_idx, fake->row_idx, val, result,
                          pg_stmt->pg_sql ? pg_stmt->pg_sql : "?");
            }

            pthread_mutex_unlock(&pg_stmt->mutex);
            return result;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return 0;
    }
    return orig_sqlite3_value_int ? orig_sqlite3_value_int(pVal) : 0;
}

// Intercept sqlite3_value_int64 to handle our fake values
// CRITICAL: Must hold mutex while accessing pg_stmt->result
sqlite3_int64 my_sqlite3_value_int64(sqlite3_value *pVal) {
    if (!pVal) return 0;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        
        pthread_mutex_lock(&pg_stmt->mutex);
        if (pg_stmt->result && fake->row_idx >= 0 && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return 0;
            }
            const char *val = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!val) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return 0;
            }
            sqlite3_int64 result;
            // Handle PostgreSQL boolean 't'/'f' format
            if (val[0] == 't' && val[1] == '\0') result = 1;
            else if (val[0] == 'f' && val[1] == '\0') result = 0;
            else result = atoll(val);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return result;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return 0;
    }
    return orig_sqlite3_value_int64 ? orig_sqlite3_value_int64(pVal) : 0;
}

// Intercept sqlite3_value_double to handle our fake values
// CRITICAL: Must hold mutex while accessing pg_stmt->result
double my_sqlite3_value_double(sqlite3_value *pVal) {
    if (!pVal) return 0.0;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        
        pthread_mutex_lock(&pg_stmt->mutex);
        if (pg_stmt->result && fake->row_idx >= 0 && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return 0.0;
            }
            const char *val = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!val) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return 0.0;
            }
            double result;
            // Handle PostgreSQL boolean 't'/'f' format
            if (val[0] == 't' && val[1] == '\0') result = 1.0;
            else if (val[0] == 'f' && val[1] == '\0') result = 0.0;
            else result = atof(val);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return result;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return 0.0;
    }
    return orig_sqlite3_value_double ? orig_sqlite3_value_double(pVal) : 0.0;
}

// Intercept sqlite3_value_bytes to handle our fake values
// CRITICAL: Must hold mutex while accessing pg_stmt->result
int my_sqlite3_value_bytes(sqlite3_value *pVal) {
    if (!pVal) return 0;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        
        pthread_mutex_lock(&pg_stmt->mutex);
        if (pg_stmt->result && fake->row_idx >= 0 && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return 0;
            }
            int len = PQgetlength(pg_stmt->result, fake->row_idx, fake->col_idx);
            pthread_mutex_unlock(&pg_stmt->mutex);
            return len;
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return 0;
    }
    return orig_sqlite3_value_bytes ? orig_sqlite3_value_bytes(pVal) : 0;
}

// Static buffers for value_blob - LARGE pool, separate from text buffers
// 64 buffers x 64KB = 4MB total - prevents race condition wrap-around
static char value_blob_buffers[64][65536];
static atomic_int value_blob_idx = 0;  // Atomic for thread-safe increment

// Intercept sqlite3_value_blob to handle our fake values
const void* my_sqlite3_value_blob(sqlite3_value *pVal) {
    if (!pVal) return NULL;  // CRITICAL FIX: NULL check
    pg_fake_value_t *fake = pg_check_fake_value(pVal);
    if (fake && fake->pg_stmt) {
        pg_stmt_t *pg_stmt = (pg_stmt_t*)fake->pg_stmt;
        pthread_mutex_lock(&pg_stmt->mutex);
        if (pg_stmt->result && fake->row_idx >= 0 && fake->row_idx < pg_stmt->num_rows && fake->col_idx < pg_stmt->num_cols) {
            if (PQgetisnull(pg_stmt->result, fake->row_idx, fake->col_idx)) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }
            // CRITICAL FIX: Copy to static buffer to prevent use-after-free
            const char *pg_value = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
            int len = PQgetlength(pg_stmt->result, fake->row_idx, fake->col_idx);
            if (!pg_value || len <= 0) {
                pthread_mutex_unlock(&pg_stmt->mutex);
                return NULL;
            }
            if (len > 65535) len = 65535;  // Truncate if too large

            // Thread-safe buffer allocation using atomic increment
            int buf = atomic_fetch_add(&value_blob_idx, 1) & 0x3F;  // % 64 via bitmask
            memcpy(value_blob_buffers[buf], pg_value, len);

            pthread_mutex_unlock(&pg_stmt->mutex);
            return value_blob_buffers[buf];
        }
        pthread_mutex_unlock(&pg_stmt->mutex);
        return NULL;
    }
    return orig_sqlite3_value_blob ? orig_sqlite3_value_blob(pVal) : NULL;
}
