/*
 * PostgreSQL Shim - Statement Module
 * Statement tracking, TLS caching, and helper functions
 */

#include "pg_statement.h"
#include "pg_logging.h"
#include "pg_config.h"
#include "pg_query_cache.h"
#include "pg_mem_telemetry.h"
#include "sql_translator.h"
#include "db_interpose_rust.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <ctype.h>
#include "shim_alloc.h"

// ============================================================================
// Static State
// ============================================================================

static pthread_key_t cached_stmts_key;
static pthread_once_t cached_stmts_key_once = PTHREAD_ONCE_INIT;
static volatile int cached_stmts_key_valid = 0;
static pthread_once_t statement_init_once = PTHREAD_ONCE_INIT;

// ============================================================================
// TLS Setup
// ============================================================================

static void stmt_ref_cb(size_t pg_stmt_ptr) {
    pg_stmt_ref((pg_stmt_t *)pg_stmt_ptr);
}

static void stmt_unref_cb(size_t pg_stmt_ptr) {
    pg_stmt_unref((pg_stmt_t *)pg_stmt_ptr);
}

static void stmt_free_cb(size_t pg_stmt_ptr) {
    pg_stmt_free((pg_stmt_t *)pg_stmt_ptr);
}

static void free_thread_cached_stmts(void *ptr) {
    if (ptr) {
        free(ptr);
    }

    int count = 0;
    size_t *stmt_ptrs = rust_cached_stmt_drain_all(&count);
    if (!stmt_ptrs || count <= 0) {
        return;
    }

    for (int i = 0; i < count; i++) {
        if (stmt_ptrs[i] != 0) {
            pg_stmt_unref((pg_stmt_t *)stmt_ptrs[i]);
        }
    }
    free(stmt_ptrs);
}

static void create_cached_stmts_key(void) {
    int rc = pthread_key_create(&cached_stmts_key, free_thread_cached_stmts);
    if (rc != 0) {
        LOG_ERROR("pthread_key_create failed with error %d", rc);
        cached_stmts_key_valid = 0;
    } else {
        cached_stmts_key_valid = 1;
    }
}

static void ensure_thread_cached_stmt_destructor(void) {
    pthread_once(&cached_stmts_key_once, create_cached_stmts_key);
    if (!cached_stmts_key_valid) {
        return;
    }

    void *sentinel = pthread_getspecific(cached_stmts_key);
    if (!sentinel) {
        sentinel = calloc(1, sizeof(int));
        if (sentinel) {
            pthread_setspecific(cached_stmts_key, sentinel);
        }
    }
}

// ============================================================================
// Initialization
// ============================================================================

static void do_statement_init(void) {
    rust_stmt_set_callbacks(stmt_ref_cb, stmt_unref_cb, stmt_free_cb);
    rust_stmt_registry_init();
    LOG_DEBUG("pg_statement initialized with Rust registry");
}

void pg_statement_init(void) {
    pthread_once(&statement_init_once, do_statement_init);
}

void pg_statement_cleanup(void) {
    rust_stmt_registry_cleanup();
}

// ============================================================================
// Statement Registry (Hash Table)
// ============================================================================

void pg_register_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt) {
    if (!sqlite_stmt || !pg_stmt) return;
    rust_stmt_register((size_t)sqlite_stmt, (size_t)pg_stmt);
}

void pg_unregister_stmt(sqlite3_stmt *sqlite_stmt) {
    if (!sqlite_stmt) return;
    rust_stmt_unregister((size_t)sqlite_stmt);
}

pg_stmt_t* pg_find_stmt(sqlite3_stmt *stmt) {
    return (pg_stmt_t *)rust_stmt_find((size_t)stmt);
}

pg_stmt_t* pg_find_any_stmt(sqlite3_stmt *stmt) {
    return (pg_stmt_t *)rust_stmt_find_any((size_t)stmt);
}

int pg_is_our_stmt(void *ptr) {
    return rust_stmt_is_ours((size_t)ptr);
}

// ============================================================================
// TLS Cached Statement Management
// ============================================================================

void pg_register_cached_stmt(sqlite3_stmt *sqlite_stmt, pg_stmt_t *pg_stmt) {
    if (!sqlite_stmt || !pg_stmt) return;
    ensure_thread_cached_stmt_destructor();
    rust_cached_stmt_register((size_t)sqlite_stmt, (size_t)pg_stmt);
}

pg_stmt_t* pg_find_cached_stmt(sqlite3_stmt *sqlite_stmt) {
    return (pg_stmt_t *)rust_cached_stmt_find((size_t)sqlite_stmt);
}

void pg_clear_cached_stmt(sqlite3_stmt *sqlite_stmt) {
    rust_cached_stmt_clear((size_t)sqlite_stmt);
}

// CRITICAL FIX: Weak clear - removes from cache without unreferencing
// Used by finalize() because global registry owns the reference
void pg_clear_cached_stmt_weak(sqlite3_stmt *sqlite_stmt) {
    rust_cached_stmt_clear_weak((size_t)sqlite_stmt);
}

// ============================================================================
// Statement Lifecycle
// ============================================================================

pg_stmt_t* pg_stmt_create(pg_connection_t *conn, const char *sql, sqlite3_stmt *shadow_stmt) {
    pg_stmt_t *stmt = calloc(1, sizeof(pg_stmt_t));
    if (!stmt) return NULL;

    // CRITICAL FIX: Use recursive mutex to prevent deadlock when bind/reset
    // operations internally trigger column functions on the same statement
    pthread_mutexattr_t attr;
    pthread_mutexattr_init(&attr);
    pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_RECURSIVE);
    pthread_mutex_init(&stmt->mutex, &attr);
    pthread_mutexattr_destroy(&attr);
    atomic_store(&stmt->ref_count, 1);  // CRITICAL FIX: Initialize ref count
    stmt->conn = conn;
    stmt->shadow_stmt = shadow_stmt;
    stmt->sql = sql ? strdup(sql) : NULL;
    stmt->current_row = -1;
    stmt->cached_row = -1;     // CRITICAL FIX: Prevent false cache hits on row 0
    stmt->decoded_blob_row = -1;  // CRITICAL FIX: Also init decoded blob row
    stmt->write_executed = 0;  // Initialize write execution guard
    stmt->read_done = 0;       // Initialize read completion guard

    return stmt;
}

// CRITICAL FIX: Reference counting to prevent double-free
void pg_stmt_ref(pg_stmt_t *stmt) {
    if (!stmt) return;
    atomic_fetch_add(&stmt->ref_count, 1);
}

void pg_stmt_unref(pg_stmt_t *stmt) {
    if (!stmt) return;

    int old = atomic_fetch_sub(&stmt->ref_count, 1);
    LOG_DEBUG("pg_stmt_unref: stmt=%p old_ref=%d new_ref=%d sql=%.40s",
              (void*)stmt, old, old-1, stmt->sql ? stmt->sql : "NULL");

    if (old <= 0) {
        // CRITICAL BUG: ref_count was already 0 or negative!
        LOG_ERROR("pg_stmt_unref: CRITICAL BUG - ref_count was %d before decrement! stmt=%p sql=%.40s",
                  old, (void*)stmt, stmt->sql ? stmt->sql : "NULL");
        LOG_ERROR("pg_stmt_unref: This indicates double-unref or missing ref. RESTORING to prevent negative.");
        // Restore ref_count to prevent it from going more negative
        atomic_store(&stmt->ref_count, 0);
        return;  // Don't free
    }

    if (old == 1) {
        // Last reference - actually free
        LOG_DEBUG("pg_stmt_unref: last reference, freeing stmt=%p", (void*)stmt);
        pg_stmt_free(stmt);
    }
}

// Helper: check if param_value points to pre-allocated buffer
static inline int is_preallocated_buffer(pg_stmt_t *stmt, int idx) {
    return stmt->param_values[idx] >= stmt->param_buffers[idx] &&
           stmt->param_values[idx] < stmt->param_buffers[idx] + 32;
}

void pg_stmt_free(pg_stmt_t *stmt) {
    if (!stmt) return;

    // CRITICAL FIX: Verify ref_count is actually 0 before freeing
    int ref_count = atomic_load(&stmt->ref_count);
    if (ref_count != 0) {
        LOG_ERROR("pg_stmt_free: WARNING ref_count=%d (expected 0) for stmt=%p sql=%.50s",
                  ref_count, (void*)stmt, stmt->sql ? stmt->sql : "NULL");
        // Don't free if ref_count > 0 - object is still in use!
        if (ref_count > 0) {
            LOG_ERROR("pg_stmt_free: ABORT - ref_count=%d, not freeing to prevent use-after-free", ref_count);
            return;
        }
    }

    // v0.9.29: Cancel + drain streaming results before freeing
    // Use PQcancel to stop server-side query first, then drain remaining results.
    // This is MUCH faster than draining thousands of PGRES_SINGLE_TUPLE rows.
    if (stmt->streaming_mode && stmt->streaming_conn) {
        pthread_mutex_lock(&stmt->streaming_conn->mutex);
        if (stmt->streaming_conn->conn) {
            // Cancel the in-progress query on the server
            PGcancel *cancel = PQgetCancel(stmt->streaming_conn->conn);
            if (cancel) {
                char errbuf[256];
                if (!PQcancel(cancel, errbuf, sizeof(errbuf))) {
                    LOG_ERROR("pg_stmt_free: PQcancel failed: %s", errbuf);
                }
                PQfreeCancel(cancel);
            }
            // Drain remaining results (should be few after cancel)
            PGresult *drain;
            int drain_count = 0;
            while ((drain = PQgetResult(stmt->streaming_conn->conn)) != NULL) {
                drain_count++;
                PQclear(drain);
                if (drain_count > 1000) {
                    LOG_INFO("pg_stmt_free: drain after cancel exceeded 1000 on %p", (void*)stmt->streaming_conn);
                    break;
                }
            }
            if (drain_count > 0) {
                LOG_DEBUG("pg_stmt_free: drained %d results after cancel", drain_count);
            }
        }
        pthread_mutex_unlock(&stmt->streaming_conn->mutex);
        stmt->streaming_mode = 0;
        atomic_store(&stmt->streaming_conn->streaming_active, 0);
        stmt->streaming_conn = NULL;
    }

    LOG_DEBUG("pg_stmt_free: START stmt=%p sql=%p pg_sql=%p",
              (void*)stmt, (void*)stmt->sql, (void*)stmt->pg_sql);

    /* Save whether pg_sql is a separate allocation BEFORE freeing sql,
     * because comparing pointers to freed memory is undefined behavior. */
    int pg_sql_is_separate = (stmt->pg_sql && stmt->pg_sql != stmt->sql);

    if (stmt->sql) {
        LOG_DEBUG("pg_stmt_free: freeing sql=%p (%.50s)", (void*)stmt->sql, stmt->sql);
        free(stmt->sql);
        stmt->sql = NULL;
    }
    if (pg_sql_is_separate) {
        LOG_DEBUG("pg_stmt_free: freeing pg_sql=%p (%.50s)", (void*)stmt->pg_sql, stmt->pg_sql);
        free(stmt->pg_sql);
        stmt->pg_sql = NULL;
    }
    if (stmt->result) {
        LOG_DEBUG("pg_stmt_free: PQclear result=%p", (void*)stmt->result);
        PQclear(stmt->result);
    }

    // Validate param_count to prevent out-of-bounds access
    int safe_param_count = stmt->param_count;
    if (safe_param_count < 0) safe_param_count = 0;
    if (safe_param_count > MAX_PARAMS) safe_param_count = MAX_PARAMS;

    // Free all captured bind values, not just up to param_count.
    // Some edge paths can temporarily populate indices beyond param_count
    // (for example when SQLite index mapping and translated count diverge).
    // Scanning MAX_PARAMS here is safe and prevents stale heap allocations
    // from surviving until process exit.
    for (int i = 0; i < MAX_PARAMS; i++) {
        // Only free if not pointing to pre-allocated buffer
        if (stmt->param_values[i] && !is_preallocated_buffer(stmt, i)) {
            LOG_DEBUG("pg_stmt_free: freeing param_values[%d]=%p", i, (void*)stmt->param_values[i]);
            free(stmt->param_values[i]);
            stmt->param_values[i] = NULL;  // Prevent double-free
            if (i >= safe_param_count && pg_mem_telemetry_enabled())
                pg_mem_telemetry_add(PMT_STMT_SWEEP_EXTRA_FREE, 0, 1);
        }
    }

    // Free parameter names (for named parameter mapping)
    if (stmt->param_names) {
        LOG_DEBUG("pg_stmt_free: freeing param_names=%p (array of %d)", (void*)stmt->param_names, safe_param_count);
        for (int i = 0; i < safe_param_count; i++) {
            if (stmt->param_names[i]) {
                LOG_DEBUG("pg_stmt_free: freeing param_names[%d]=%p (%.30s)",
                          i, (void*)stmt->param_names[i], stmt->param_names[i]);
                free(stmt->param_names[i]);
                stmt->param_names[i] = NULL;  // Prevent double-free
            }
        }
        LOG_DEBUG("pg_stmt_free: freeing param_names array at %p", (void*)stmt->param_names);
        free(stmt->param_names);
        stmt->param_names = NULL;
    }

    // Free decoded blob cache
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->decoded_blobs[i]) {
            LOG_DEBUG("pg_stmt_free: freeing decoded_blobs[%d]=%p", i, (void*)stmt->decoded_blobs[i]);
            free(stmt->decoded_blobs[i]);
            stmt->decoded_blobs[i] = NULL;
        }
    }

    // Free cached text and blob
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->cached_text[i]) {
            LOG_DEBUG("pg_stmt_free: freeing cached_text[%d]=%p", i, (void*)stmt->cached_text[i]);
            free(stmt->cached_text[i]);
            stmt->cached_text[i] = NULL;
        }
        if (stmt->cached_blob[i]) {
            LOG_DEBUG("pg_stmt_free: freeing cached_blob[%d]=%p", i, (void*)stmt->cached_blob[i]);
            free(stmt->cached_blob[i]);
            stmt->cached_blob[i] = NULL;
        }
    }

    // Free resolved column table names
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->col_table_names[i]) {
            free(stmt->col_table_names[i]);
            stmt->col_table_names[i] = NULL;
        }
    }

    // Free column names from PQdescribePrepared
    if (stmt->col_names) {
        for (int i = 0; i < stmt->num_col_names; i++) {
            if (stmt->col_names[i]) {
                free(stmt->col_names[i]);
            }
        }
        free(stmt->col_names);
        stmt->col_names = NULL;
        stmt->num_col_names = 0;
    }

    LOG_DEBUG("pg_stmt_free: destroying mutex and freeing stmt=%p", (void*)stmt);
    pthread_mutex_destroy(&stmt->mutex);
    free(stmt);
    LOG_DEBUG("pg_stmt_free: DONE");
}

void pg_stmt_clear_result(pg_stmt_t *stmt) {
    if (!stmt) return;

    // v0.9.29: Cancel + drain streaming results before clearing
    // Use PQcancel to stop server-side query first, then drain remaining results.
    if (stmt->streaming_mode && stmt->streaming_conn) {
        pthread_mutex_lock(&stmt->streaming_conn->mutex);
        if (stmt->streaming_conn->conn) {
            // Cancel the in-progress query on the server
            PGcancel *cancel = PQgetCancel(stmt->streaming_conn->conn);
            if (cancel) {
                char errbuf[256];
                if (!PQcancel(cancel, errbuf, sizeof(errbuf))) {
                    LOG_ERROR("pg_stmt_clear_result: PQcancel failed: %s", errbuf);
                }
                PQfreeCancel(cancel);
            }
            // Drain remaining results (should be few after cancel)
            PGresult *drain;
            int drain_count = 0;
            while ((drain = PQgetResult(stmt->streaming_conn->conn)) != NULL) {
                drain_count++;
                PQclear(drain);
                if (drain_count > 1000) {
                    LOG_INFO("pg_stmt_clear_result: drain after cancel exceeded 1000 on %p", (void*)stmt->streaming_conn);
                    break;
                }
            }
            if (drain_count > 0) {
                LOG_DEBUG("pg_stmt_clear_result: drained %d results after cancel (sql=%.60s)",
                         drain_count, stmt->sql ? stmt->sql : "?");
            }
        }
        pthread_mutex_unlock(&stmt->streaming_conn->mutex);
        stmt->streaming_mode = 0;
        atomic_store(&stmt->streaming_conn->streaming_active, 0);
        stmt->streaming_conn = NULL;
    }

    if (stmt->result) {
        PQclear(stmt->result);
        stmt->result = NULL;
    }
    // Release cached result ref before clearing pointer
    if (stmt->cached_result) {
        pg_query_cache_release(stmt->cached_result);
        stmt->cached_result = NULL;
    }
    stmt->current_row = -1;
    stmt->num_rows = 0;
    stmt->num_cols = 0;
    stmt->write_executed = 0;  // Reset write execution guard
    stmt->read_done = 0;       // Reset read completion guard

    // Clear decoded blob cache
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->decoded_blobs[i]) {
            free(stmt->decoded_blobs[i]);
            stmt->decoded_blobs[i] = NULL;
            stmt->decoded_blob_lens[i] = 0;
        }
    }
    stmt->decoded_blob_row = -1;

    // Free cached text and blob on clear
    for (int i = 0; i < MAX_PARAMS; i++) {
        if (stmt->cached_text[i]) {
            free(stmt->cached_text[i]);
            stmt->cached_text[i] = NULL;
        }
        if (stmt->cached_blob[i]) {
            free(stmt->cached_blob[i]);
            stmt->cached_blob[i] = NULL;
            stmt->cached_blob_len[i] = 0;
        }
    }
    stmt->cached_row = -1;

    // Free column names from PQdescribePrepared (they belong to the previous result)
    if (stmt->col_names) {
        for (int i = 0; i < stmt->num_col_names; i++) {
            if (stmt->col_names[i]) {
                free(stmt->col_names[i]);
            }
        }
        free(stmt->col_names);
        stmt->col_names = NULL;
        stmt->num_col_names = 0;
    }
}

// ============================================================================
// SQL Transformation Helpers
// ============================================================================

char* convert_metadata_settings_insert_to_upsert(const char *sql) {
    return rust_convert_metadata_settings_upsert(sql);
}

sqlite3_int64 extract_metadata_id_from_generator_sql(const char *sql) {
    return (sqlite3_int64)rust_extract_metadata_id(sql);
}

// ============================================================================
// Fake sqlite3_value Helpers
// ============================================================================

int pg_oid_to_sqlite_type(Oid oid) {
    return rust_oid_to_sqlite_type((unsigned int)oid);
}

// Convert PostgreSQL OID to SQLite declared type string.
// Returns a pointer to a static string owned by the Rust module; do not free.
// OID 20 (int8/bigint) returns "BIGINT" — critical for SOCI 64-bit handling.
const char* pg_oid_to_sqlite_decltype(Oid oid) {
    return rust_oid_to_sqlite_decltype((unsigned int)oid);
}

int pg_decltype_special_case(Oid oid, const char *col_name, const char *pg_sql, Oid table_oid) {
    return rust_decltype_special_case((unsigned int)oid, col_name, pg_sql, (unsigned int)table_oid);
}

sqlite3_value* pg_create_column_value(pg_stmt_t *stmt, int col_idx) {
    int sqlite_type = SQLITE_NULL;
    if (!stmt || !stmt->result || stmt->current_row < 0 ||
        stmt->current_row >= stmt->num_rows ||
        PQgetisnull(stmt->result, stmt->current_row, col_idx)) {
        sqlite_type = SQLITE_NULL;
    } else {
        sqlite_type = pg_oid_to_sqlite_type(PQftype(stmt->result, col_idx));
    }

    return (sqlite3_value*)rust_create_column_value((size_t)stmt, col_idx, sqlite_type);
}

int pg_is_our_value(sqlite3_value *val) {
    return rust_is_our_value((const pg_value_t *)val);
}
