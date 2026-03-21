/*
 * PostgreSQL Shim - Logging Module
 * Thread-safe logging with configurable levels
 */

#ifndef PG_LOGGING_H
#define PG_LOGGING_H

#include <stdarg.h>
#include <stdio.h>

// Log levels
typedef enum {
    PG_LOG_ERROR = 0,
    PG_LOG_INFO = 1,
    PG_LOG_DEBUG = 2
} pg_log_level_t;

// Rust logging helpers (implemented in plex-pg-core)
int rust_logging_get_level(void);
void rust_logging_write(int level, const char *message);

// Initialize logging (called automatically on first log)
void pg_logging_init(void);

// Cleanup logging
void pg_logging_cleanup(void);

// Reset logging after fork (called by atfork handler)
// Reinitializes mutex and reopens log file for child process
void pg_logging_reset_after_fork(void);

// Core logging function (internal - use macros below)
static inline void pg_log_message_internal(int level, const char *fmt, ...) {
    if (level > rust_logging_get_level()) {
        return;
    }

    char buf[4096];
    va_list args;
    va_start(args, fmt);
    vsnprintf(buf, sizeof(buf), fmt, args);
    va_end(args);

    rust_logging_write(level, buf);
}

// Log SQL fallback for analysis
void log_sql_fallback(const char *original_sql, const char *translated_sql,
                      const char *error_msg, const char *context);

// Check if error is a known translation limitation
int is_known_translation_limitation(const char *error_msg);

// Convenience macros
#define LOG_ERROR(fmt, ...) pg_log_message_internal(PG_LOG_ERROR, fmt, ##__VA_ARGS__)
#define LOG_INFO(fmt, ...)  pg_log_message_internal(PG_LOG_INFO, fmt, ##__VA_ARGS__)
#define LOG_DEBUG(fmt, ...) pg_log_message_internal(PG_LOG_DEBUG, fmt, ##__VA_ARGS__)

#endif // PG_LOGGING_H
