/*
 * PostgreSQL Shim - Query Result Cache Implementation
 *
 * Thread-local cache for query results to avoid hitting PostgreSQL
 * for repeated identical queries (common in Plex's OnDeck endpoint).
 */

#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <stdatomic.h>
#include <libpq-fe.h>

#include "pg_query_cache.h"
#include "pg_types.h"
#include "pg_logging.h"
#include "shim_alloc.h"

// Rust FFI: pure helpers migrated to rust/sql-translator/src/pg_query_cache.rs
extern uint64_t rust_fnv1a_hash(const void *data, size_t len);
extern uint64_t rust_get_time_ms(void);

// Thread-local cache
static pthread_key_t cache_key;
static pthread_once_t cache_key_once = PTHREAD_ONCE_INIT;

// Free a single cached result (only if ref_count is 0)
static void free_cached_result(cached_result_t *entry) {
    if (!entry || entry->cache_key == 0) return;

    // Don't free if still referenced
    if (atomic_load(&entry->ref_count) > 0) {
        LOG_DEBUG("CACHE_FREE_SKIP: entry still has %d refs", atomic_load(&entry->ref_count));
        return;
    }

    // Free column types
    if (entry->col_types) {
        free(entry->col_types);
        entry->col_types = NULL;
    }

    // Free column names
    if (entry->col_names) {
        for (int i = 0; i < entry->num_cols; i++) {
            if (entry->col_names[i]) free(entry->col_names[i]);
        }
        free(entry->col_names);
        entry->col_names = NULL;
    }

    // Free rows
    if (entry->rows) {
        for (int r = 0; r < entry->num_rows; r++) {
            cached_row_t *row = &entry->rows[r];
            if (row->values) {
                for (int c = 0; c < entry->num_cols; c++) {
                    if (row->values[c]) free(row->values[c]);
                }
                free(row->values);
            }
            if (row->lengths) free(row->lengths);
            if (row->is_null) free(row->is_null);
        }
        free(entry->rows);
        entry->rows = NULL;
    }

    entry->cache_key = 0;
    entry->num_rows = 0;
    entry->num_cols = 0;
}

// Thread-local destructor
static void cache_destructor(void *ptr) {
    query_cache_t *cache = (query_cache_t *)ptr;
    if (!cache) return;

    // Log stats before cleanup
    if (cache->total_hits > 0 || cache->total_misses > 0) {
        LOG_INFO("QUERY_CACHE thread exit: hits=%llu misses=%llu ratio=%.1f%%",
                 (unsigned long long)cache->total_hits,
                 (unsigned long long)cache->total_misses,
                 cache->total_hits + cache->total_misses > 0 ?
                     100.0 * cache->total_hits / (cache->total_hits + cache->total_misses) : 0);
    }

    // Free all cached results
    for (int i = 0; i < QUERY_CACHE_SIZE; i++) {
        free_cached_result(&cache->entries[i]);
    }

    free(cache);
}

// Create thread-local key
static void create_cache_key(void) {
    pthread_key_create(&cache_key, cache_destructor);
}

// Get or create thread-local cache
static query_cache_t* get_thread_cache(void) {
    pthread_once(&cache_key_once, create_cache_key);

    query_cache_t *cache = pthread_getspecific(cache_key);
    if (!cache) {
        cache = calloc(1, sizeof(query_cache_t));
        if (cache) {
            pthread_setspecific(cache_key, cache);
        }
    }
    return cache;
}

// ============================================================================
// Public API
// ============================================================================

void pg_query_cache_init(void) {
    // Just ensure key is created
    pthread_once(&cache_key_once, create_cache_key);
    LOG_INFO("Query result cache initialized (size=%d, ttl=%dms)",
             QUERY_CACHE_SIZE, QUERY_CACHE_TTL_MS);
}

void pg_query_cache_cleanup(void) {
    // Thread-local cleanup happens via destructor
}

uint64_t pg_query_cache_key(pg_stmt_t *stmt) {
    if (!stmt || !stmt->pg_sql) return 0;

    // Start with SQL hash
    uint64_t hash = rust_fnv1a_hash(stmt->pg_sql, strlen(stmt->pg_sql));

    // Mix in parameter values
    for (int i = 0; i < stmt->param_count && i < MAX_PARAMS; i++) {
        if (stmt->param_values[i]) {
            // Hash the parameter value
            uint64_t param_hash = rust_fnv1a_hash(stmt->param_values[i],
                                                   strlen(stmt->param_values[i]));
            // Mix hashes
            hash ^= param_hash;
            hash *= 0x100000001b3ULL;
        } else {
            // NULL parameter - use a sentinel value
            hash ^= 0xDEADBEEFULL;
            hash *= 0x100000001b3ULL;
        }
    }

    return hash;
}

cached_result_t* pg_query_cache_lookup(pg_stmt_t *stmt) {
    LOG_DEBUG("CACHE_LOOKUP_ENTER: stmt=%p", (void*)stmt);
    if (!stmt || !stmt->pg_sql) {
        LOG_DEBUG("CACHE_LOOKUP_EARLY_EXIT: stmt or pg_sql is NULL");
        return NULL;
    }
    LOG_DEBUG("CACHE_LOOKUP: calling get_thread_cache");

    query_cache_t *cache = get_thread_cache();
    LOG_DEBUG("CACHE_LOOKUP: got cache=%p", (void*)cache);
    if (!cache) return NULL;

    uint64_t key = pg_query_cache_key(stmt);
    if (key == 0) return NULL;

    uint64_t now = rust_get_time_ms();

    // Linear search (cache is small)
    for (int i = 0; i < QUERY_CACHE_SIZE; i++) {
        cached_result_t *entry = &cache->entries[i];

        if (entry->cache_key == key) {
            // Check TTL
            if (now - entry->created_ms < QUERY_CACHE_TTL_MS) {
                // Cache hit! Increment ref_count to prevent eviction
                atomic_fetch_add(&entry->ref_count, 1);
                entry->hit_count++;
                cache->total_hits++;

                // Log every 100th hit to reduce spam
                if (entry->hit_count % 100 == 1) {
                    LOG_DEBUG("QUERY_CACHE HIT #%d: key=%llx rows=%d refs=%d sql=%.60s",
                             entry->hit_count, (unsigned long long)key,
                             entry->num_rows, atomic_load(&entry->ref_count), stmt->pg_sql);
                }

                return entry;
            } else {
                // Expired - try to free (will skip if ref_count > 0)
                free_cached_result(entry);
                cache->total_misses++;
                return NULL;
            }
        }
    }

    cache->total_misses++;
    return NULL;
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

    // Don't cache huge results
    if (num_rows > QUERY_CACHE_MAX_ROWS) {
        LOG_DEBUG("QUERY_CACHE SKIP: too many rows (%d > %d)", num_rows, QUERY_CACHE_MAX_ROWS);
        return;
    }

    // Don't cache empty results (usually not worth it)
    if (num_rows == 0) {
        LOG_DEBUG("QUERY_CACHE SKIP: empty result");
        return;
    }

    query_cache_t *cache = get_thread_cache();
    if (!cache) return;

    uint64_t key = pg_query_cache_key(stmt);
    if (key == 0) return;

    // Find slot - either existing with same key, oldest, or first free
    int target_slot = -1;
    uint64_t oldest_time = UINT64_MAX;

    for (int i = 0; i < QUERY_CACHE_SIZE; i++) {
        cached_result_t *entry = &cache->entries[i];

        // Exact match - reuse slot
        if (entry->cache_key == key) {
            target_slot = i;
            break;
        }

        // Free slot
        if (entry->cache_key == 0) {
            target_slot = i;
            break;
        }

        // LRU candidate
        if (entry->created_ms < oldest_time) {
            oldest_time = entry->created_ms;
            target_slot = i;
        }
    }

    if (target_slot < 0) {
        target_slot = 0;  // Fallback to first slot
    }

    // Free existing entry
    free_cached_result(&cache->entries[target_slot]);

    cached_result_t *entry = &cache->entries[target_slot];

    // Calculate total size estimate
    size_t total_size = 0;

    // Allocate column types
    entry->col_types = malloc(num_cols * sizeof(Oid));
    if (!entry->col_types) goto cleanup;

    // Allocate column names
    entry->col_names = calloc(num_cols, sizeof(char*));
    if (!entry->col_names) goto cleanup;

    for (int c = 0; c < num_cols; c++) {
        entry->col_types[c] = PQftype(result, c);
        const char *name = PQfname(result, c);
        if (name) {
            entry->col_names[c] = strdup(name);
            total_size += strlen(name) + 1;
        }
    }

    // Allocate rows
    entry->rows = calloc(num_rows, sizeof(cached_row_t));
    if (!entry->rows) goto cleanup;

    for (int r = 0; r < num_rows; r++) {
        cached_row_t *row = &entry->rows[r];

        row->values = calloc(num_cols, sizeof(char*));
        row->lengths = calloc(num_cols, sizeof(int));
        row->is_null = calloc(num_cols, sizeof(int));

        if (!row->values || !row->lengths || !row->is_null) goto cleanup;

        for (int c = 0; c < num_cols; c++) {
            if (PQgetisnull(result, r, c)) {
                row->is_null[c] = 1;
                row->values[c] = NULL;
                row->lengths[c] = 0;
            } else {
                int len = PQgetlength(result, r, c);
                row->lengths[c] = len;
                row->is_null[c] = 0;

                // Check size limit
                total_size += len + 1;
                if (total_size > QUERY_CACHE_MAX_BYTES) {
                    LOG_DEBUG("QUERY_CACHE SKIP: result too large (%zu > %d bytes)",
                             total_size, QUERY_CACHE_MAX_BYTES);
                    goto cleanup;
                }

                // Copy value (including null terminator for strings)
                row->values[c] = malloc(len + 1);
                if (!row->values[c]) goto cleanup;

                const char *val = PQgetvalue(result, r, c);
                memcpy(row->values[c], val, len);
                row->values[c][len] = '\0';
            }
        }
    }

    // Success - fill in metadata
    entry->cache_key = key;
    entry->created_ms = rust_get_time_ms();
    atomic_store(&entry->ref_count, 0);  // Initialize ref_count
    entry->num_rows = num_rows;
    entry->num_cols = num_cols;
    entry->hit_count = 0;
    cache->count++;

    LOG_DEBUG("QUERY_CACHE STORE: key=%llx rows=%d cols=%d size=%zu sql=%.60s",
             (unsigned long long)key, num_rows, num_cols, total_size, stmt->pg_sql);
    return;

cleanup:
    // Failed to allocate - clean up partial entry
    free_cached_result(entry);
}

void pg_query_cache_invalidate(pg_stmt_t *stmt) {
    if (!stmt || !stmt->pg_sql) return;

    query_cache_t *cache = get_thread_cache();
    if (!cache) return;

    uint64_t key = pg_query_cache_key(stmt);
    if (key == 0) return;

    for (int i = 0; i < QUERY_CACHE_SIZE; i++) {
        if (cache->entries[i].cache_key == key) {
            free_cached_result(&cache->entries[i]);
            cache->count--;
            return;
        }
    }
}

void pg_query_cache_stats(uint64_t *hits, uint64_t *misses) {
    query_cache_t *cache = get_thread_cache();
    if (!cache) {
        if (hits) *hits = 0;
        if (misses) *misses = 0;
        return;
    }

    if (hits) *hits = cache->total_hits;
    if (misses) *misses = cache->total_misses;
}

void pg_query_cache_release(cached_result_t *entry) {
    if (!entry) return;

    int old_count = atomic_fetch_sub(&entry->ref_count, 1);
    if (old_count <= 1) {
        // ref_count is now 0 or less - entry can be freed on next eviction
        LOG_DEBUG("CACHE_RELEASE: entry %p now has 0 refs, eligible for eviction",
                 (void*)entry);
    }
}
