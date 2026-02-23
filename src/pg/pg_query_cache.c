/*
 * PostgreSQL Shim - Query Result Cache Implementation
 *
 * Thread-local cache for query results to avoid hitting PostgreSQL
 * for repeated identical queries (common in Plex's OnDeck endpoint).
 *
 * Phase 3 migration: All state management, TLS lifecycle, and memory
 * ownership is now handled by the Rust module in
 * rust/plex-pg-core/src/pg_query_cache.rs
 *
 * This C file is a thin shim that:
 *   1. Extracts fields from pg_stmt_t (which Rust cannot access directly
 *      without reproducing the full struct layout)
 *   2. Extracts data from PGresult via libpq calls
 *   3. Forwards both to the Rust FFI functions
 *
 * The cached_result_t struct is #[repr(C)] in Rust with the exact same
 * layout as the C definition in pg_types.h, so C callers in
 * db_interpose_column.c and db_interpose_step.c can read fields directly
 * through the pointer returned by pg_query_cache_lookup().
 */

#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <stdatomic.h>
#include <libpq-fe.h>

#include "pg_query_cache.h"
#include "pg_types.h"
#include "pg_logging.h"
#include "db_interpose_rust.h"
#include "shim_alloc.h"

// ============================================================================
// Public API — thin C shims forwarding to Rust
// ============================================================================

void pg_query_cache_init(void) {
    rust_query_cache_init();
}

void pg_query_cache_cleanup(void) {
    rust_query_cache_cleanup();
}

uint64_t pg_query_cache_key(pg_stmt_t *stmt) {
    if (!stmt || !stmt->pg_sql) return 0;

    // Extract fields from pg_stmt_t and pass to Rust
    return rust_query_cache_key(
        stmt->pg_sql,
        (const char *const *)stmt->param_values,
        stmt->param_count
    );
}

cached_result_t* pg_query_cache_lookup(pg_stmt_t *stmt) {
    LOG_DEBUG("CACHE_LOOKUP_ENTER: stmt=%p", (void*)stmt);
    if (!stmt || !stmt->pg_sql) {
        LOG_DEBUG("CACHE_LOOKUP_EARLY_EXIT: stmt or pg_sql is NULL");
        return NULL;
    }

    uint64_t key = pg_query_cache_key(stmt);
    if (key == 0) return NULL;

    cached_result_t *result = rust_query_cache_lookup(key);
    if (result) {
        LOG_DEBUG("QUERY_CACHE HIT: key=%llx rows=%d refs=%d sql=%.60s",
                  (unsigned long long)key, result->num_rows,
                  atomic_load(&result->ref_count), stmt->pg_sql);
    }
    return result;
}

void pg_query_cache_store(pg_stmt_t *stmt, void *result_ptr) {
    LOG_DEBUG("CACHE_STORE_ENTER: stmt=%p result=%p", (void*)stmt, result_ptr);
    PGresult *result = (PGresult *)result_ptr;
    if (!stmt || !stmt->pg_sql || !result) {
        LOG_DEBUG("CACHE_STORE_EARLY_EXIT: null check failed");
        return;
    }

    // Don't cache failed queries
    ExecStatusType status = PQresultStatus(result);
    LOG_DEBUG("CACHE_STORE: status=%d", (int)status);
    if (status != PGRES_TUPLES_OK) return;

    int num_rows = PQntuples(result);
    int num_cols = PQnfields(result);
    LOG_DEBUG("CACHE_STORE: rows=%d cols=%d max=%d", num_rows, num_cols, QUERY_CACHE_MAX_ROWS);

    // Early exit for results that won't be cached
    if (num_rows > QUERY_CACHE_MAX_ROWS || num_rows == 0 || num_cols == 0) {
        return;
    }

    uint64_t key = pg_query_cache_key(stmt);
    if (key == 0) return;

    // Extract data from PGresult into flat arrays for Rust
    // Allocate temporary arrays on the stack/heap for the FFI call
    Oid *col_types = malloc(num_cols * sizeof(Oid));
    const char **col_names = malloc(num_cols * sizeof(char*));
    if (!col_types || !col_names) {
        free(col_types);
        free(col_names);
        return;
    }

    for (int c = 0; c < num_cols; c++) {
        col_types[c] = PQftype(result, c);
        col_names[c] = PQfname(result, c);  // Points into PGresult — valid until PQclear
    }

    // Flatten row data into contiguous arrays
    int total = num_rows * num_cols;
    const char **values = malloc(total * sizeof(char*));
    int *lengths = malloc(total * sizeof(int));
    int *is_null = malloc(total * sizeof(int));
    if (!values || !lengths || !is_null) {
        free(col_types);
        free(col_names);
        free(values);
        free(lengths);
        free(is_null);
        return;
    }

    for (int r = 0; r < num_rows; r++) {
        for (int c = 0; c < num_cols; c++) {
            int idx = r * num_cols + c;
            if (PQgetisnull(result, r, c)) {
                is_null[idx] = 1;
                values[idx] = NULL;
                lengths[idx] = 0;
            } else {
                is_null[idx] = 0;
                lengths[idx] = PQgetlength(result, r, c);
                values[idx] = PQgetvalue(result, r, c);  // Points into PGresult
            }
        }
    }

    // Forward to Rust — Rust will copy all data into its own allocation
    rust_query_cache_store(
        key, num_rows, num_cols,
        col_types, col_names,
        values, lengths, is_null,
        stmt->pg_sql
    );

    // Free temporary extraction arrays (Rust has already copied the data)
    free(col_types);
    free(col_names);
    free(values);
    free(lengths);
    free(is_null);
}

void pg_query_cache_invalidate(pg_stmt_t *stmt) {
    if (!stmt || !stmt->pg_sql) return;

    uint64_t key = pg_query_cache_key(stmt);
    if (key == 0) return;

    rust_query_cache_invalidate(key);
}

void pg_query_cache_stats(uint64_t *hits, uint64_t *misses) {
    rust_query_cache_stats(hits, misses);
}

void pg_query_cache_release(cached_result_t *entry) {
    rust_query_cache_release(entry);
}
