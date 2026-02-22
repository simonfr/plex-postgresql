/*
 * sql_translator_rust_bridge.c
 *
 * Sole implementation of sql_translate() — delegates entirely to the Rust
 * sqlparser-rs translator (libsql_translator.a).
 *
 * The Rust FFI exposes:
 *   SqlTranslation sql_translator_translate_full(const char *sql);
 *   void           sql_translator_free(char *ptr);
 *   void           sql_translator_translation_free(SqlTranslation *t);
 *
 * SqlTranslation (from ffi.rs):
 *   char    *sql;           // heap-allocated, free with sql_translator_free()
 *   int32_t  param_count;
 *   char   **param_names;   // heap array of param_count C strings (or NULLs)
 *   int32_t  success;       // 1 = ok, 0 = error
 *   uint8_t  error[256];
 */

#include "../include/sql_translator.h"
#include "pg_logging.h"
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <stdio.h>

/* ── Rust FFI declarations ─────────────────────────────────────────────────── */

typedef struct {
    char    *sql;
    int      param_count;
    char   **param_names;
    int      success;
    char     error[256];
} RustSqlTranslation;

extern RustSqlTranslation sql_translator_translate_full(const char *sql);
extern void               sql_translator_free(char *ptr);
extern void               sql_translator_translation_free(RustSqlTranslation *t);

/* ── Thread-local translation cache ────────────────────────────────────────── */

#include <pthread.h>

#define CACHE_SIZE 256

typedef struct {
    uint64_t hash;
    char    *input;
    char    *sql;
    char   **param_names;
    int      param_count;
} cache_entry_t;

typedef struct {
    cache_entry_t entries[CACHE_SIZE];
    int           inited;
} trans_cache_t;

static pthread_key_t   trans_cache_key;
static pthread_once_t  trans_cache_once = PTHREAD_ONCE_INIT;

/* Free a single cache entry's heap data */
static void free_cache_entry(cache_entry_t *e) {
    free(e->input);
    e->input = NULL;
    free(e->sql);
    e->sql = NULL;
    if (e->param_names) {
        for (int i = 0; i < e->param_count; i++)
            free(e->param_names[i]);
        free(e->param_names);
        e->param_names = NULL;
    }
    e->param_count = 0;
    e->hash = 0;
}

/* TLS destructor: called on thread exit to free all cached translations */
static void trans_cache_destructor(void *ptr) {
    trans_cache_t *tc = (trans_cache_t *)ptr;
    if (!tc) return;
    for (int i = 0; i < CACHE_SIZE; i++) {
        if (tc->entries[i].input)
            free_cache_entry(&tc->entries[i]);
    }
    free(tc);
}

static void create_trans_cache_key(void) {
    pthread_key_create(&trans_cache_key, trans_cache_destructor);
}

static trans_cache_t *get_thread_cache(void) {
    pthread_once(&trans_cache_once, create_trans_cache_key);
    trans_cache_t *tc = (trans_cache_t *)pthread_getspecific(trans_cache_key);
    if (!tc) {
        tc = (trans_cache_t *)calloc(1, sizeof(trans_cache_t));
        if (tc) {
            tc->inited = 1;
            pthread_setspecific(trans_cache_key, tc);
        }
    }
    return tc;
}

static uint64_t hash_sql(const char *sql) {
    /* FNV-1a 64-bit */
    uint64_t h = 14695981039346656037ULL;
    for (const char *p = sql; *p; p++) {
        h ^= (uint64_t)(unsigned char)*p;
        h *= 1099511628211ULL;
    }
    return h;
}

static cache_entry_t *cache_lookup(const char *sql, uint64_t hash) {
    trans_cache_t *tc = get_thread_cache();
    if (!tc || !tc->inited) return NULL;
    int idx = (int)(hash % CACHE_SIZE);
    cache_entry_t *e = &tc->entries[idx];
    if (e->input && e->hash == hash && strcmp(e->input, sql) == 0) {
        return e;
    }
    return NULL;
}

static void cache_store(const char *input, uint64_t hash,
                        const char *sql, int param_count, char **param_names) {
    trans_cache_t *tc = get_thread_cache();
    if (!tc) return;

    int idx = (int)(hash % CACHE_SIZE);
    cache_entry_t *e = &tc->entries[idx];

    /* Evict old entry */
    free_cache_entry(e);

    e->hash = hash;
    e->input = strdup(input);
    e->sql = strdup(sql);
    e->param_count = param_count;
    if (param_names && param_count > 0) {
        e->param_names = malloc(param_count * sizeof(char*));
        for (int i = 0; i < param_count; i++)
            e->param_names[i] = param_names[i] ? strdup(param_names[i]) : NULL;
    } else {
        e->param_names = NULL;
    }
}

/* ── sql_translate() — main entry point ────────────────────────────────────── */

sql_translation_t sql_translate(const char *sqlite_sql) {
    sql_translation_t result = {0};

    if (!sqlite_sql) {
        strcpy(result.error, "NULL input SQL");
        return result;
    }

    /* Thread-local cache lookup */
    uint64_t hash = hash_sql(sqlite_sql);
    cache_entry_t *cached = cache_lookup(sqlite_sql, hash);
    if (cached) {
        result.sql = strdup(cached->sql);
        result.param_count = cached->param_count;
        if (cached->param_names && cached->param_count > 0) {
            result.param_names = malloc(cached->param_count * sizeof(char*));
            if (result.param_names) {
                for (int i = 0; i < cached->param_count; i++)
                    result.param_names[i] = cached->param_names[i] ? strdup(cached->param_names[i]) : NULL;
            }
        } else {
            result.param_names = NULL;
        }
        result.success = 1;
        return result;
    }

    /* Cache miss — call Rust translator */
    RustSqlTranslation rust = sql_translator_translate_full(sqlite_sql);

    if (!rust.success) {
        snprintf(result.error, sizeof(result.error), "%.*s",
                 (int)sizeof(result.error) - 1, rust.error);
        LOG_ERROR("sql_translate failed: %.100s — %s", sqlite_sql, result.error);
        return result;
    }

    /* Copy sql (strdup since caller frees with free()) */
    result.sql = rust.sql ? strdup(rust.sql) : NULL;

    /* Copy param_names */
    result.param_count = rust.param_count;
    if (rust.param_names && rust.param_count > 0) {
        result.param_names = malloc(rust.param_count * sizeof(char*));
        if (result.param_names) {
            for (int i = 0; i < rust.param_count; i++) {
                char *name = rust.param_names[i];
                result.param_names[i] = name ? strdup(name) : NULL;
            }
        }
    } else {
        result.param_names = NULL;
    }

    /* Free Rust-owned memory */
    sql_translator_translation_free(&rust);

    result.success = 1;

    /* Store in thread-local cache */
    cache_store(sqlite_sql, hash, result.sql, result.param_count, result.param_names);

    return result;
}

/* ── sql_translation_free() ────────────────────────────────────────────────── */

void sql_translation_free(sql_translation_t *result) {
    if (!result) return;

    if (result->sql) {
        free(result->sql);
        result->sql = NULL;
    }

    if (result->param_names) {
        for (int i = 0; i < result->param_count; i++) {
            if (result->param_names[i])
                free(result->param_names[i]);
        }
        free(result->param_names);
        result->param_names = NULL;
    }

    result->param_count = 0;
    result->success = 0;
}

/* ── Init / cleanup stubs ──────────────────────────────────────────────────── */

void sql_translator_init(void) {
    LOG_INFO("sql_translator: Rust (sqlparser-rs) backend active");
}

void sql_translator_cleanup(void) {
    /* Thread-local cache is cleaned up automatically */
}
