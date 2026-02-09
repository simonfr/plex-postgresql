/*
 * Plex PostgreSQL Interposing Shim - Core Module (macOS)
 *
 * This is the macOS-specific entry point containing:
 * - fishhook-based symbol interception
 * - macOS-specific backtrace/exception handling
 * - Constructor/destructor
 *
 * Common code is in db_interpose_common.c
 */

#include "db_interpose.h"
#include "db_interpose_common.h"
#include "pg_query_cache.h"
#include "fishhook.h"
#include <signal.h>

// ============================================================================
// C++ Exception Interception (macOS via fishhook)
// ============================================================================

// Original __cxa_throw function pointer - must use noreturn attribute to match ABI
typedef void (*cxa_throw_fn)(void*, void*, void(*)(void*)) __attribute__((noreturn));
static cxa_throw_fn orig_cxa_throw = NULL;

// Thread-local recursion prevention
static __thread int in_exception_handler = 0;

// Thread-local counters and demangle function are in db_interpose_common.c
// platform_print_backtrace is in platform_backtrace.c

// ============================================================================
// Exception Handling (macOS - currently disabled via fishhook)
// ============================================================================

// Our __cxa_throw interceptor - MUST be noreturn to match original ABI
// NOTE: Currently disabled in fishhook rebindings due to ABI issues
__attribute__((noreturn))
static void my_cxa_throw(void *thrown_exception, void *tinfo, void (*dest)(void*)) {
    int should_call_original = 1;
    
    // Use common exception handling logic
    if (!common_handle_exception(thrown_exception, tinfo, &in_exception_handler, &should_call_original)) {
        // Recursion detected
        if (orig_cxa_throw) {
            orig_cxa_throw(thrown_exception, tinfo, dest);
        }
        abort();
    }
    
    // Call original - MUST call this for exception to propagate correctly
    if (orig_cxa_throw) {
        orig_cxa_throw(thrown_exception, tinfo, dest);
    }
    
    // Should never reach here
    abort();
}

// Signal handler uses common implementation from db_interpose_common.c

// ============================================================================
// fishhook Rebinding Setup (macOS-specific)
// ============================================================================

static void setup_fishhook_rebindings(void) {
    fprintf(stderr, "[SHIM_INIT] Setting up fishhook rebindings...\n");

    struct rebinding rebindings[] = {
        // Open/Close
        {"sqlite3_open", my_sqlite3_open, (void**)&orig_sqlite3_open},
        {"sqlite3_open_v2", my_sqlite3_open_v2, (void**)&orig_sqlite3_open_v2},
        {"sqlite3_close", my_sqlite3_close, (void**)&orig_sqlite3_close},
        {"sqlite3_close_v2", my_sqlite3_close_v2, (void**)&orig_sqlite3_close_v2},

        // Exec
        {"sqlite3_exec", my_sqlite3_exec, (void**)&orig_sqlite3_exec},
        {"sqlite3_get_table", my_sqlite3_get_table, (void**)&orig_sqlite3_get_table},

        // Metadata
        {"sqlite3_changes", my_sqlite3_changes, (void**)&orig_sqlite3_changes},
        {"sqlite3_changes64", my_sqlite3_changes64, (void**)&orig_sqlite3_changes64},
        {"sqlite3_last_insert_rowid", my_sqlite3_last_insert_rowid, (void**)&orig_sqlite3_last_insert_rowid},
        {"sqlite3_errmsg", my_sqlite3_errmsg, (void**)&orig_sqlite3_errmsg},
        {"sqlite3_errcode", my_sqlite3_errcode, (void**)&orig_sqlite3_errcode},
        {"sqlite3_extended_errcode", my_sqlite3_extended_errcode, (void**)&orig_sqlite3_extended_errcode},

        // Prepare
        {"sqlite3_prepare", my_sqlite3_prepare, (void**)&orig_sqlite3_prepare},
        {"sqlite3_prepare_v2", my_sqlite3_prepare_v2, (void**)&orig_sqlite3_prepare_v2},
        {"sqlite3_prepare_v3", my_sqlite3_prepare_v3, (void**)&orig_sqlite3_prepare_v3},
        {"sqlite3_prepare16_v2", my_sqlite3_prepare16_v2, (void**)&orig_sqlite3_prepare16_v2},

        // Bind
        {"sqlite3_bind_int", my_sqlite3_bind_int, (void**)&orig_sqlite3_bind_int},
        {"sqlite3_bind_int64", my_sqlite3_bind_int64, (void**)&orig_sqlite3_bind_int64},
        {"sqlite3_bind_double", my_sqlite3_bind_double, (void**)&orig_sqlite3_bind_double},
        {"sqlite3_bind_text", my_sqlite3_bind_text, (void**)&orig_sqlite3_bind_text},
        {"sqlite3_bind_text64", my_sqlite3_bind_text64, (void**)&orig_sqlite3_bind_text64},
        {"sqlite3_bind_blob", my_sqlite3_bind_blob, (void**)&orig_sqlite3_bind_blob},
        {"sqlite3_bind_blob64", my_sqlite3_bind_blob64, (void**)&orig_sqlite3_bind_blob64},
        {"sqlite3_bind_value", my_sqlite3_bind_value, (void**)&orig_sqlite3_bind_value},
        {"sqlite3_bind_null", my_sqlite3_bind_null, (void**)&orig_sqlite3_bind_null},

        // Step/Reset/Finalize
        {"sqlite3_step", my_sqlite3_step, (void**)&orig_sqlite3_step},
        {"sqlite3_reset", my_sqlite3_reset, (void**)&orig_sqlite3_reset},
        {"sqlite3_finalize", my_sqlite3_finalize, (void**)&orig_sqlite3_finalize},
        {"sqlite3_clear_bindings", my_sqlite3_clear_bindings, (void**)&orig_sqlite3_clear_bindings},

        // Column access
        {"sqlite3_column_count", my_sqlite3_column_count, (void**)&orig_sqlite3_column_count},
        {"sqlite3_column_type", my_sqlite3_column_type, (void**)&orig_sqlite3_column_type},
        {"sqlite3_column_int", my_sqlite3_column_int, (void**)&orig_sqlite3_column_int},
        {"sqlite3_column_int64", my_sqlite3_column_int64, (void**)&orig_sqlite3_column_int64},
        {"sqlite3_column_double", my_sqlite3_column_double, (void**)&orig_sqlite3_column_double},
        {"sqlite3_column_text", my_sqlite3_column_text, (void**)&orig_sqlite3_column_text},
        {"sqlite3_column_blob", my_sqlite3_column_blob, (void**)&orig_sqlite3_column_blob},
        {"sqlite3_column_bytes", my_sqlite3_column_bytes, (void**)&orig_sqlite3_column_bytes},
        {"sqlite3_column_name", my_sqlite3_column_name, (void**)&orig_sqlite3_column_name},
        {"sqlite3_column_decltype", my_sqlite3_column_decltype, (void**)&orig_sqlite3_column_decltype},
        {"sqlite3_column_value", my_sqlite3_column_value, (void**)&orig_sqlite3_column_value},
        {"sqlite3_data_count", my_sqlite3_data_count, (void**)&orig_sqlite3_data_count},

        // Value access
        {"sqlite3_value_type", my_sqlite3_value_type, (void**)&orig_sqlite3_value_type},
        {"sqlite3_value_text", my_sqlite3_value_text, (void**)&orig_sqlite3_value_text},
        {"sqlite3_value_int", my_sqlite3_value_int, (void**)&orig_sqlite3_value_int},
        {"sqlite3_value_int64", my_sqlite3_value_int64, (void**)&orig_sqlite3_value_int64},
        {"sqlite3_value_double", my_sqlite3_value_double, (void**)&orig_sqlite3_value_double},
        {"sqlite3_value_bytes", my_sqlite3_value_bytes, (void**)&orig_sqlite3_value_bytes},
        {"sqlite3_value_blob", my_sqlite3_value_blob, (void**)&orig_sqlite3_value_blob},

        // Collation
        {"sqlite3_create_collation", my_sqlite3_create_collation, (void**)&orig_sqlite3_create_collation},
        {"sqlite3_create_collation_v2", my_sqlite3_create_collation_v2, (void**)&orig_sqlite3_create_collation_v2},

        // Memory and statement info
        {"sqlite3_free", my_sqlite3_free, (void**)&orig_sqlite3_free},
        {"sqlite3_malloc", my_sqlite3_malloc, (void**)&orig_sqlite3_malloc},
        {"sqlite3_db_handle", my_sqlite3_db_handle, (void**)&orig_sqlite3_db_handle},
        {"sqlite3_sql", my_sqlite3_sql, (void**)&orig_sqlite3_sql},
        {"sqlite3_expanded_sql", my_sqlite3_expanded_sql, (void**)&orig_sqlite3_expanded_sql},
        {"sqlite3_bind_parameter_count", my_sqlite3_bind_parameter_count, (void**)&orig_sqlite3_bind_parameter_count},
        {"sqlite3_bind_parameter_index", my_sqlite3_bind_parameter_index, (void**)&orig_sqlite3_bind_parameter_index},
        {"sqlite3_stmt_readonly", my_sqlite3_stmt_readonly, (void**)&orig_sqlite3_stmt_readonly},
        {"sqlite3_stmt_busy", my_sqlite3_stmt_busy, (void**)&orig_sqlite3_stmt_busy},
        {"sqlite3_stmt_status", my_sqlite3_stmt_status, (void**)&orig_sqlite3_stmt_status},
        {"sqlite3_bind_parameter_name", my_sqlite3_bind_parameter_name, (void**)&orig_sqlite3_bind_parameter_name},
        
        // C++ exception interception DISABLED - causes crash regardless of noreturn
        // {"__cxa_throw", (void*)my_cxa_throw, (void**)&orig_cxa_throw},
    };

    int count = sizeof(rebindings) / sizeof(rebindings[0]);
    int result = rebind_symbols(rebindings, count);

    if (result == 0) {
        fprintf(stderr, "[SHIM_INIT] fishhook rebind_symbols succeeded for %d functions\n", count);
    } else {
        fprintf(stderr, "[SHIM_INIT] ERROR: fishhook rebind_symbols failed with code %d\n", result);
    }
}

// ============================================================================
// SQLite Fallback Loading (macOS paths)
// ============================================================================

static void load_sqlite_fallback(void) {
    const char *sqlite_paths[] = {
        "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3_orig.dylib",
        "/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3.dylib",
        "/usr/lib/libsqlite3.dylib",
        NULL
    };

    for (int i = 0; sqlite_paths[i] != NULL && sqlite_handle == NULL; i++) {
        sqlite_handle = dlopen(sqlite_paths[i], RTLD_LAZY | RTLD_LOCAL);
        if (sqlite_handle) {
            fprintf(stderr, "[SHIM_INIT] Loaded SQLite fallback from: %s\n", sqlite_paths[i]);
            break;
        }
    }

    // If fishhook didn't set up pointers, use dlsym as fallback
    if (sqlite_handle && (!real_sqlite3_prepare_v2 || !orig_sqlite3_prepare_v2)) {
        fprintf(stderr, "[SHIM_INIT] Fishhook incomplete, using dlsym fallback\n");
        common_load_sqlite_symbols(sqlite_handle);
    }
}

// Lazy init for ensure_real_sqlite_loaded
void ensure_real_sqlite_loaded(void) {
    if (real_sqlite3_prepare_v2) return;
    
    if (!sqlite_handle) {
        load_sqlite_fallback();
    }
    
    if (sqlite_handle) {
        real_sqlite3_prepare_v2 = dlsym(sqlite_handle, "sqlite3_prepare_v2");
        real_sqlite3_errmsg = dlsym(sqlite_handle, "sqlite3_errmsg");
        real_sqlite3_errcode = dlsym(sqlite_handle, "sqlite3_errcode");
    }
}

// ============================================================================
// Constructor/Destructor (macOS)
// ============================================================================

// shim_init_pid is in db_interpose_common.c

__attribute__((constructor))
static void shim_init(void) {
    fprintf(stderr, "[SHIM_INIT] Constructor starting (macOS)...\n");
    fflush(stderr);
    
    // Detect fork and reset state if needed
    common_check_fork();

    // Install signal handlers (only in debug mode)
    #ifdef DEBUG
    signal(SIGSEGV, common_signal_handler);
    signal(SIGABRT, common_signal_handler);
    signal(SIGBUS, common_signal_handler);
    signal(SIGFPE, common_signal_handler);
    signal(SIGILL, common_signal_handler);
    #endif

    // Install fork handlers
    pthread_atfork(common_atfork_prepare, common_atfork_parent, common_atfork_child);
    fprintf(stderr, "[SHIM_INIT] Registered pthread_atfork handlers\n");
    fflush(stderr);

    pg_logging_init();
    LOG_INFO("=== Plex PostgreSQL Interpose Shim loaded (macOS) ===");

    fprintf(stderr, "[SHIM_INIT] Logging initialized\n");
    fflush(stderr);

    // Use fishhook to rebind SQLite symbols
    setup_fishhook_rebindings();

    // Load SQLite fallback
    load_sqlite_fallback();

    // Initialize common modules
    common_shim_init_modules();

    shim_initialized = 1;

    fprintf(stderr, "[SHIM_INIT] Constructor complete (macOS, PID %d)\n", getpid());
    fflush(stderr);
}

__attribute__((destructor))
static void shim_cleanup(void) {
    if (!shim_initialized) return;

    LOG_INFO("=== Plex PostgreSQL Interpose Shim unloading (macOS) ===");
    common_shim_cleanup();
}
