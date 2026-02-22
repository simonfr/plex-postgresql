/*
 * sql_translator_rust_bridge.c
 *
 * Optional bridge: when PLEX_SQL_TRANSLATOR=rust is set at runtime,
 * sql_translate() delegates to the Rust sqlparser-rs based translator
 * (libsql_translator.a) instead of the C string-rewriting pipeline.
 *
 * The Rust translator exposes:
 *   SqlTranslation sql_translator_translate_full(const char *sql);
 *   void           sql_translator_free(char *ptr);
 *
 * SqlTranslation (from ffi.rs):
 *   char   *sql;          // heap-allocated C string, free with sql_translator_free()
 *   int32_t param_count;
 *   int32_t success;      // 1 = ok, 0 = error
 *   uint8_t error[256];   // null-terminated error message
 *
 * Callers should check use_rust_translator() at init time and cache the result.
 */

#include "../include/sql_translator.h"
#include "pg_logging.h"
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

/* ── Rust FFI declarations ─────────────────────────────────────────────────── */

typedef struct {
    char    *sql;
    int      param_count;
    int      success;
    char     error[256];
} RustSqlTranslation;

extern RustSqlTranslation sql_translator_translate_full(const char *sql);
extern void               sql_translator_free(char *ptr);

/* ── Runtime feature flag ──────────────────────────────────────────────────── */

int use_rust_translator(void) {
    static int cached = -1;
    if (cached < 0) {
        const char *env = getenv("PLEX_SQL_TRANSLATOR");
        cached = (env && strcasecmp(env, "rust") == 0) ? 1 : 0;
        if (cached) {
            LOG_INFO("sql_translator: using Rust (sqlparser-rs) backend");
        }
    }
    return cached;
}

/* ── Bridge function ───────────────────────────────────────────────────────── */

/*
 * sql_translate_via_rust() — translates SQLite SQL to PostgreSQL SQL using
 * the Rust sqlparser-rs backend.
 *
 * Returns a sql_translation_t with the same semantics as sql_translate():
 *   - result.sql       is malloc'd; caller must call sql_translation_free().
 *   - result.success   is 1 on success, 0 on failure.
 *   - result.param_names / param_count: the Rust translator currently returns
 *     param_count but not the individual names (only positional $1/$2 params
 *     are emitted). Named params (:name) are not used by Plex at runtime, only
 *     during shadow-SQLite dummy prepare — so param_names is left NULL here
 *     and the C cache/caller handles that gracefully.
 */
sql_translation_t sql_translate_via_rust(const char *sqlite_sql) {
    sql_translation_t result = {0};

    if (!sqlite_sql) {
        snprintf(result.error, sizeof(result.error), "NULL input SQL");
        return result;
    }

    RustSqlTranslation rust = sql_translator_translate_full(sqlite_sql);

    if (!rust.success) {
        snprintf(result.error, sizeof(result.error), "%.*s",
                 (int)sizeof(result.error) - 1, rust.error);
        /* rust.sql is NULL on failure, nothing to free */
        LOG_ERROR("Rust translator failed for: %.100s — %s", sqlite_sql, result.error);
        return result;
    }

    /* Transfer ownership: copy the Rust-allocated string into a C malloc'd copy
     * so that sql_translation_free() can call free() on it safely.
     * (Rust's CString allocator and C's malloc are the same on macOS/Linux.) */
    result.sql = rust.sql ? strdup(rust.sql) : NULL;
    sql_translator_free(rust.sql);   /* free the Rust-owned copy */

    result.param_count = rust.param_count;
    result.param_names = NULL;   /* positional only — see comment above */
    result.success = 1;
    return result;
}
