/*
 * PostgreSQL Shim - Logging Module (thin C shim)
 * Variadic printf formatting stays in C; all heavy lifting delegated to Rust.
 */

#include "pg_logging.h"
#include <stdio.h>
#include <stdarg.h>

/* Rust FFI declarations */
extern void rust_logging_init(void);
extern int  rust_logging_get_level(void);
extern void rust_logging_write(int level, const char *message);
extern void rust_logging_fallback(const char *original_sql, const char *translated_sql,
                                   const char *error_msg, const char *context);
extern int  rust_logging_is_known_limitation(const char *error_msg);
extern void rust_logging_reset_after_fork(void);
extern void rust_logging_cleanup(void);

/* ── Public API implementations ── */

void pg_logging_init(void) {
    rust_logging_init();
}

void pg_logging_cleanup(void) {
    rust_logging_cleanup();
}

void pg_logging_reset_after_fork(void) {
    rust_logging_reset_after_fork();
}

/* The variadic entry point — format in C, write via Rust */
void pg_log_message_internal(int level, const char *fmt, ...) {
    /* Fast path: check level before any work */
    if (level > rust_logging_get_level()) return;

    char buf[4096];
    va_list args;
    va_start(args, fmt);
    vsnprintf(buf, sizeof(buf), fmt, args);
    va_end(args);

    rust_logging_write(level, buf);
}

/* Non-variadic — direct delegation */
void log_sql_fallback(const char *original_sql, const char *translated_sql,
                      const char *error_msg, const char *context) {
    rust_logging_fallback(original_sql, translated_sql, error_msg, context);
}

int is_known_translation_limitation(const char *error_msg) {
    return rust_logging_is_known_limitation(error_msg);
}
