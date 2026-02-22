/*
 * SQL Translator - SQLite to PostgreSQL
 *
 * Handles translation of SQLite-specific SQL syntax to PostgreSQL.
 * Part of the Plex PostgreSQL Adapter.
 */

#ifndef SQL_TRANSLATOR_H
#define SQL_TRANSLATOR_H

#include <stddef.h>
#include <strings.h>  // for strncasecmp

// ============================================================================
// Safe strcasestr implementation
// musl's strcasestr has issues with certain inputs, so we use our own
// ============================================================================

char* safe_strcasestr(const char *haystack, const char *needle);

// Replace system strcasestr with our safe version
#ifdef strcasestr
#undef strcasestr
#endif
#define strcasestr safe_strcasestr

// Translation result
typedef struct {
    char *sql;              // Translated SQL (caller must free)
    char **param_names;     // Original named parameter names (for :name params)
    int param_count;        // Number of parameters
    int success;            // 1 if translation succeeded
    char error[256];        // Error message if failed
} sql_translation_t;

// Initialize translator (call once at startup)
void sql_translator_init(void);

// Runtime backend selection
int use_rust_translator(void);

// Rust backend (when PLEX_SQL_TRANSLATOR=rust)
sql_translation_t sql_translate_via_rust(const char *sqlite_sql);

// Cleanup translator
void sql_translator_cleanup(void);

// Main translation function
sql_translation_t sql_translate(const char *sqlite_sql);

// Free translation result
void sql_translation_free(sql_translation_t *result);

// Individual translation functions (for testing/debugging)
char* sql_translate_placeholders(const char *sql, char ***param_names, int *param_count);
char* sql_translate_functions(const char *sql);
char* sql_translate_types(const char *sql);
char* sql_translate_keywords(const char *sql);

// Utility
void sql_translator_free(char *sql);

#endif /* SQL_TRANSLATOR_H */
