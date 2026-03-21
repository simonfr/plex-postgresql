/*
 * Plex PostgreSQL Interposing Shim - Common Header
 *
 * Declarations for platform-independent code shared between macOS and Linux.
 */

#ifndef DB_INTERPOSE_COMMON_H
#define DB_INTERPOSE_COMMON_H

#include <pthread.h>
#include <sys/types.h>  // for pid_t

// ============================================================================
// Exception Tracking (used by platform-specific exception handlers)
// ============================================================================

typedef struct {
    const char *type_name;
    int count;
    int logged_with_trace;
} exception_type_tracker_t;

// Exception tracking limits
#define MAX_EXCEPTION_TYPES 64
#define MAX_LOGGED_PER_TYPE 3
#define MAX_LOGGED_TOTAL 50

// Demangle function pointer (defined in Rust common module)
extern char* (*cxa_demangle_fn)(const char*, char*, size_t*, int*);

// Process ID for fork detection (defined in Rust common module)
extern pid_t shim_init_pid;

// Exception tracking functions
exception_type_tracker_t* get_exception_tracker(const char *type_name);
void reset_exception_tracking(void);
const char* get_type_name(void *tinfo);

// ============================================================================
// Symbol Verification
// ============================================================================

void reset_symbol_verification(void);

// ============================================================================
// Platform-Specific Backtrace (implemented in platform core files)
// ============================================================================

// Print a backtrace to stderr with the given reason
// skip_frames: number of stack frames to skip (for hiding internal calls)
void platform_print_backtrace(const char *reason, int skip_frames);

// ============================================================================
// Common Signal Handler
// ============================================================================

// Common signal handler that prints context and backtrace
void common_signal_handler(int sig);

// ============================================================================
// Common Exception Info Printing
// ============================================================================

// Print exception information with shim context
// Returns the demangled type name (caller must free if non-NULL)
char* print_exception_info(const char *type_name,
                           int count,
                           void *thrown_exception,
                           void *tinfo);

// ============================================================================
// Common Exception Handler Logic
// ============================================================================

// Thread-local recursion guard (defined in platform files, declared here for sharing)
// Each platform defines its own static __thread in_exception_handler

// Handle exception logging and decide whether to log/trace
// Returns: 1 if exception was handled (caller should continue), 0 if recursion detected
// Sets *should_call_original to 1 if original throw should be called
// The caller is responsible for calling the original __cxa_throw after this returns
int common_handle_exception(void *thrown_exception, void *tinfo, 
                           int *in_handler_flag,
                           int *should_call_original);

// Lightweight accessors for exception diagnostics from C++ helpers.
const char* pg_exception_get_last_query(void);
const char* pg_exception_get_last_column(void);

// Track recent SQL for exception diagnostics.
void pg_exception_note_query(const char *sql);
void pg_exception_dump_recent_queries(void);

// Track execution phases for exception diagnostics.
void pg_exception_note_phase(const char *phase,
                             const char *sql,
                             const void *stmt,
                             const void *db);
void pg_exception_dump_recent_phases(void);

// ============================================================================
// Fork Handlers (called from platform-specific code)
// ============================================================================

void common_atfork_prepare(void);
void common_atfork_parent(void);
void common_atfork_child(void);

// ============================================================================
// Common Initialization/Cleanup
// ============================================================================

// Check if we're in a forked process and reset state if needed
// Returns 1 if fork was detected, 0 otherwise
int common_check_fork(void);

// Initialize all common modules (pg_config, pg_client, pg_statement, etc.)
void common_shim_init_modules(void);

// Cleanup all common modules
void common_shim_cleanup(void);

// ============================================================================
// Shared Symbol Loading
// ============================================================================

// Populate all orig_sqlite3_* function pointers via dlsym from the given handle.
// Uses if-not-set pattern: only sets pointers that are still NULL.
// Called from macOS load_sqlite_fallback() and Linux load_original_functions().
void common_load_sqlite_symbols(void *handle);

#endif /* DB_INTERPOSE_COMMON_H */
