/*
 * SQL Translator - SQLite to PostgreSQL
 *
 * Translates SQLite-specific SQL syntax to PostgreSQL using the Rust
 * sqlparser-rs based translator. Part of the Plex PostgreSQL Adapter.
 */

#ifndef SQL_TRANSLATOR_H
#define SQL_TRANSLATOR_H

#include <stddef.h>

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

// Cleanup translator
void sql_translator_cleanup(void);

// Main translation function
sql_translation_t sql_translate(const char *sqlite_sql);

// Free translation result
void sql_translation_free(sql_translation_t *result);

#endif /* SQL_TRANSLATOR_H */
