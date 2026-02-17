/*
 * Plex PostgreSQL Interposing Shim - Common Module
 *
 * Platform-independent code shared between macOS and Linux.
 * Contains:
 * - Worker thread implementation
 * - Helper functions (fake value check, path helpers, string utils)
 * - Fork handlers
 * - Exception tracking
 * - Signal handler
 * - Global state definitions
 */

#include "db_interpose.h"
#include "db_interpose_common.h"
#include "pg_query_cache.h"
#include <signal.h>

// ============================================================================
// Global State Definitions (exported via db_interpose.h)
// ============================================================================

// Recursion prevention
__thread int in_interpose_call = 0;
__thread int prepare_v2_depth = 0;
__thread int in_resolve_tables = 0;  // Prevent recursion in resolve_column_tables

// SQLite library handle for dlsym fallback
void *sqlite_handle = NULL;

// Original SQLite function pointers
// On macOS: populated by fishhook rebind_symbols
// On Linux: populated by dlsym RTLD_NEXT
VISIBLE int (*orig_sqlite3_open)(const char*, sqlite3**) = NULL;
VISIBLE int (*orig_sqlite3_open_v2)(const char*, sqlite3**, int, const char*) = NULL;
VISIBLE int (*orig_sqlite3_close)(sqlite3*) = NULL;
VISIBLE int (*orig_sqlite3_close_v2)(sqlite3*) = NULL;
VISIBLE int (*orig_sqlite3_exec)(sqlite3*, const char*, int(*)(void*,int,char**,char**), void*, char**) = NULL;
VISIBLE int (*orig_sqlite3_changes)(sqlite3*) = NULL;
VISIBLE sqlite3_int64 (*orig_sqlite3_changes64)(sqlite3*) = NULL;
VISIBLE sqlite3_int64 (*orig_sqlite3_last_insert_rowid)(sqlite3*) = NULL;
VISIBLE int (*orig_sqlite3_get_table)(sqlite3*, const char*, char***, int*, int*, char**) = NULL;

VISIBLE const char* (*orig_sqlite3_errmsg)(sqlite3*) = NULL;
VISIBLE int (*orig_sqlite3_errcode)(sqlite3*) = NULL;
VISIBLE int (*orig_sqlite3_extended_errcode)(sqlite3*) = NULL;

VISIBLE int (*orig_sqlite3_prepare)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
VISIBLE int (*orig_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
VISIBLE int (*orig_sqlite3_prepare_v3)(sqlite3*, const char*, int, unsigned int, sqlite3_stmt**, const char**) = NULL;
VISIBLE int (*orig_sqlite3_prepare16_v2)(sqlite3*, const void*, int, sqlite3_stmt**, const void**) = NULL;

VISIBLE int (*orig_sqlite3_bind_int)(sqlite3_stmt*, int, int) = NULL;
VISIBLE int (*orig_sqlite3_bind_int64)(sqlite3_stmt*, int, sqlite3_int64) = NULL;
VISIBLE int (*orig_sqlite3_bind_double)(sqlite3_stmt*, int, double) = NULL;
VISIBLE int (*orig_sqlite3_bind_text)(sqlite3_stmt*, int, const char*, int, void(*)(void*)) = NULL;
VISIBLE int (*orig_sqlite3_bind_text64)(sqlite3_stmt*, int, const char*, sqlite3_uint64, void(*)(void*), unsigned char) = NULL;
VISIBLE int (*orig_sqlite3_bind_blob)(sqlite3_stmt*, int, const void*, int, void(*)(void*)) = NULL;
VISIBLE int (*orig_sqlite3_bind_blob64)(sqlite3_stmt*, int, const void*, sqlite3_uint64, void(*)(void*)) = NULL;
VISIBLE int (*orig_sqlite3_bind_value)(sqlite3_stmt*, int, const sqlite3_value*) = NULL;
VISIBLE int (*orig_sqlite3_bind_null)(sqlite3_stmt*, int) = NULL;

VISIBLE int (*orig_sqlite3_step)(sqlite3_stmt*) = NULL;
VISIBLE int (*orig_sqlite3_reset)(sqlite3_stmt*) = NULL;
VISIBLE int (*orig_sqlite3_finalize)(sqlite3_stmt*) = NULL;
VISIBLE int (*orig_sqlite3_clear_bindings)(sqlite3_stmt*) = NULL;

VISIBLE int (*orig_sqlite3_column_count)(sqlite3_stmt*) = NULL;
VISIBLE int (*orig_sqlite3_column_type)(sqlite3_stmt*, int) = NULL;
VISIBLE int (*orig_sqlite3_column_int)(sqlite3_stmt*, int) = NULL;
VISIBLE sqlite3_int64 (*orig_sqlite3_column_int64)(sqlite3_stmt*, int) = NULL;
VISIBLE double (*orig_sqlite3_column_double)(sqlite3_stmt*, int) = NULL;
VISIBLE const unsigned char* (*orig_sqlite3_column_text)(sqlite3_stmt*, int) = NULL;
VISIBLE const void* (*orig_sqlite3_column_blob)(sqlite3_stmt*, int) = NULL;
VISIBLE int (*orig_sqlite3_column_bytes)(sqlite3_stmt*, int) = NULL;
VISIBLE const char* (*orig_sqlite3_column_name)(sqlite3_stmt*, int) = NULL;
VISIBLE const char* (*orig_sqlite3_column_decltype)(sqlite3_stmt*, int) = NULL;
VISIBLE sqlite3_value* (*orig_sqlite3_column_value)(sqlite3_stmt*, int) = NULL;
VISIBLE int (*orig_sqlite3_data_count)(sqlite3_stmt*) = NULL;

VISIBLE int (*orig_sqlite3_value_type)(sqlite3_value*) = NULL;
VISIBLE const unsigned char* (*orig_sqlite3_value_text)(sqlite3_value*) = NULL;
VISIBLE int (*orig_sqlite3_value_int)(sqlite3_value*) = NULL;
VISIBLE sqlite3_int64 (*orig_sqlite3_value_int64)(sqlite3_value*) = NULL;
VISIBLE double (*orig_sqlite3_value_double)(sqlite3_value*) = NULL;
VISIBLE int (*orig_sqlite3_value_bytes)(sqlite3_value*) = NULL;
VISIBLE const void* (*orig_sqlite3_value_blob)(sqlite3_value*) = NULL;

VISIBLE int (*orig_sqlite3_create_collation)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*)) = NULL;
VISIBLE int (*orig_sqlite3_create_collation_v2)(sqlite3*, const char*, int, void*, int(*)(void*,int,const void*,int,const void*), void(*)(void*)) = NULL;

// New SQLite API functions
VISIBLE void (*orig_sqlite3_free)(void*) = NULL;
VISIBLE void* (*orig_sqlite3_malloc)(int) = NULL;
VISIBLE sqlite3* (*orig_sqlite3_db_handle)(sqlite3_stmt*) = NULL;
VISIBLE const char* (*orig_sqlite3_sql)(sqlite3_stmt*) = NULL;
VISIBLE char* (*orig_sqlite3_expanded_sql)(sqlite3_stmt*) = NULL;
VISIBLE int (*orig_sqlite3_bind_parameter_count)(sqlite3_stmt*) = NULL;
VISIBLE int (*orig_sqlite3_bind_parameter_index)(sqlite3_stmt*, const char*) = NULL;
VISIBLE int (*orig_sqlite3_stmt_readonly)(sqlite3_stmt*) = NULL;
VISIBLE int (*orig_sqlite3_stmt_busy)(sqlite3_stmt*) = NULL;
VISIBLE int (*orig_sqlite3_stmt_status)(sqlite3_stmt*, int, int) = NULL;
VISIBLE const char* (*orig_sqlite3_bind_parameter_name)(sqlite3_stmt*, int) = NULL;

// Aliases for backward compatibility (used by prepare module)
int (*real_sqlite3_prepare_v2)(sqlite3*, const char*, int, sqlite3_stmt**, const char**) = NULL;
const char* (*real_sqlite3_errmsg)(sqlite3*) = NULL;
int (*real_sqlite3_errcode)(sqlite3*) = NULL;

// Worker thread state
pthread_t worker_thread;
pthread_mutex_t worker_mutex = PTHREAD_MUTEX_INITIALIZER;
pthread_cond_t worker_cond_request = PTHREAD_COND_INITIALIZER;
pthread_cond_t worker_cond_response = PTHREAD_COND_INITIALIZER;
worker_request_t worker_request;
volatile int worker_running = 0;

// Fake value pool for sqlite3_column_value
pg_fake_value_t fake_value_pool[MAX_FAKE_VALUES];
unsigned int fake_value_next = 0;
pthread_mutex_t fake_value_mutex = PTHREAD_MUTEX_INITIALIZER;

// Initialization flag
int shim_initialized = 0;
int shim_passthrough_only = 0;

// Global context tracking for exception debugging
VISIBLE const char * volatile last_query_being_processed = NULL;
VISIBLE const char * volatile last_column_being_accessed = NULL;

// Global counters for debugging
volatile long global_value_type_calls = 0;
volatile long global_column_type_calls = 0;

// Thread-local counters for exception debugging
__thread long tls_value_type_calls = 0;
__thread long tls_column_type_calls = 0;
__thread const char *tls_last_query = NULL;

// Demangle function pointer (shared between platforms)
char* (*cxa_demangle_fn)(const char*, char*, size_t*, int*) = NULL;

// Track process ID for fork detection
pid_t shim_init_pid = 0;

// ============================================================================
// Exception Tracking
// ============================================================================

static exception_type_tracker_t exception_types[MAX_EXCEPTION_TYPES];
static int exception_type_count = 0;
volatile int total_exception_count = 0;
pthread_mutex_t exception_tracker_mutex = PTHREAD_MUTEX_INITIALIZER;

// Find or create tracker for an exception type
exception_type_tracker_t* get_exception_tracker(const char *type_name) {
    pthread_mutex_lock(&exception_tracker_mutex);

    // Look for existing tracker
    for (int i = 0; i < exception_type_count; i++) {
        if (exception_types[i].type_name == type_name ||
            (exception_types[i].type_name && type_name &&
             strcmp(exception_types[i].type_name, type_name) == 0)) {
            exception_types[i].count++;
            pthread_mutex_unlock(&exception_tracker_mutex);
            return &exception_types[i];
        }
    }

    // Create new tracker if space available
    if (exception_type_count < MAX_EXCEPTION_TYPES) {
        exception_type_tracker_t *tracker = &exception_types[exception_type_count++];
        tracker->type_name = type_name;
        tracker->count = 1;
        tracker->logged_with_trace = 0;
        pthread_mutex_unlock(&exception_tracker_mutex);
        return tracker;
    }

    pthread_mutex_unlock(&exception_tracker_mutex);
    return NULL;
}

// Reset exception tracking (called after fork)
void reset_exception_tracking(void) {
    total_exception_count = 0;
    exception_type_count = 0;
}

// Get type name from type_info (C++ ABI)
const char* get_type_name(void *tinfo) {
    if (!tinfo) return "unknown";
    // type_info layout: vtable pointer, then const char* name
    const char **name_ptr = (const char**)((char*)tinfo + sizeof(void*));
    return *name_ptr ? *name_ptr : "unknown";
}

// ============================================================================
// Helper Functions
// ============================================================================

// Check if a pointer is one of our fake values
pg_fake_value_t* pg_check_fake_value(sqlite3_value *pVal) {
    if (!pVal) return NULL;

    // Check if pointer is in our pool
    uintptr_t ptr = (uintptr_t)pVal;
    uintptr_t pool_start = (uintptr_t)&fake_value_pool[0];
    uintptr_t pool_end = (uintptr_t)&fake_value_pool[MAX_FAKE_VALUES];

    if (ptr >= pool_start && ptr < pool_end) {
        pg_fake_value_t *fake = (pg_fake_value_t*)pVal;
        if (fake->magic == PG_FAKE_VALUE_MAGIC) {
            return fake;
        }
    }
    return NULL;
}

// Helper to check if path is a Plex library database (library.db OR blobs.db)
// v0.9.5: Include blobs.db for full PostgreSQL migration
int is_library_db_path(const char *path) {
    if (!path) return 0;
    // Match both library.db and library.blobs.db
    return strstr(path, "com.plexapp.plugins.library.db") != NULL ||
           strstr(path, "com.plexapp.plugins.library.blobs.db") != NULL;
}

// Helper: check if path is for blobs.db
int is_blobs_db_path(const char *path) {
    if (!path) return 0;
    return strstr(path, "com.plexapp.plugins.library.blobs.db") != NULL;
}

// Helper: rewrite schema_migrations -> blobs_schema_migrations for blobs.db connections.
// DISABLED: blobs.db and library.db share the same schema_migrations table in PG.
// The blobs_schema_migrations table doesn't exist and isn't needed — both databases
// use identical migration versions.
char* rewrite_blobs_schema_migrations(const char *sql, const char *db_path) {
    (void)sql; (void)db_path;
    return NULL;  // No rewrite needed — shared schema_migrations table
#if 0  // Original code kept for reference
    if (!sql || !db_path) return NULL;
    if (!is_blobs_db_path(db_path)) return NULL;
    if (!strcasestr(sql, "schema_migrations")) return NULL;
    // Don't double-rewrite
    if (strcasestr(sql, "blobs_schema_migrations")) return NULL;

    // Replace all occurrences of schema_migrations with blobs_schema_migrations
    // Handle both quoted and unquoted forms
    const char *patterns[] = {
        "schema_migrations",
        NULL
    };

    char *result = strdup(sql);
    if (!result) return NULL;

    for (int i = 0; patterns[i]; i++) {
        char *pos;
        while ((pos = strcasestr(result, patterns[i])) != NULL) {
            // Make sure we don't match "blobs_schema_migrations" (already rewritten)
            if (pos > result && *(pos - 1) != ' ' && *(pos - 1) != '"' &&
                *(pos - 1) != '\'' && *(pos - 1) != '`' && *(pos - 1) != '(' &&
                *(pos - 1) != ',' && *(pos - 1) != '\t' && *(pos - 1) != '\n') {
                break; // Part of another identifier
            }
            size_t old_len = strlen(patterns[i]);
            size_t new_len = old_len + 6; // "blobs_" prefix
            size_t result_len = strlen(result);
            char *new_result = malloc(result_len + 6 + 1);
            if (!new_result) { free(result); return NULL; }
            size_t prefix_len = pos - result;
            memcpy(new_result, result, prefix_len);
            memcpy(new_result + prefix_len, "blobs_", 6);
            memcpy(new_result + prefix_len + 6, pos, strlen(pos) + 1);
            free(result);
            result = new_result;
        }
    }

    // If nothing changed, free and return NULL
    if (strcmp(result, sql) == 0) {
        free(result);
        return NULL;
    }

    LOG_INFO("BLOBS_REWRITE: %s", result);
    return result;
#endif
}

// Simple string replace helper
char* simple_str_replace(const char *str, const char *old, const char *new_str) {
    if (!str || !old || !new_str) return NULL;

    const char *pos = strstr(str, old);
    if (!pos) return NULL;

    size_t old_len = strlen(old);
    size_t new_len = strlen(new_str);
    size_t result_len = strlen(str) - old_len + new_len;

    char *result = malloc(result_len + 1);
    if (!result) return NULL;

    size_t prefix_len = pos - str;
    memcpy(result, str, prefix_len);
    memcpy(result + prefix_len, new_str, new_len);
    strcpy(result + prefix_len + new_len, pos + old_len);

    return result;
}

// ============================================================================
// Shared Symbol Loading
// ============================================================================

void common_load_sqlite_symbols(void *handle) {
    if (!handle) return;

    // Open/Close
    if (!orig_sqlite3_open) orig_sqlite3_open = dlsym(handle, "sqlite3_open");
    if (!orig_sqlite3_open_v2) orig_sqlite3_open_v2 = dlsym(handle, "sqlite3_open_v2");
    if (!orig_sqlite3_close) orig_sqlite3_close = dlsym(handle, "sqlite3_close");
    if (!orig_sqlite3_close_v2) orig_sqlite3_close_v2 = dlsym(handle, "sqlite3_close_v2");

    // Exec
    if (!orig_sqlite3_exec) orig_sqlite3_exec = dlsym(handle, "sqlite3_exec");
    if (!orig_sqlite3_get_table) orig_sqlite3_get_table = dlsym(handle, "sqlite3_get_table");

    // Metadata
    if (!orig_sqlite3_changes) orig_sqlite3_changes = dlsym(handle, "sqlite3_changes");
    if (!orig_sqlite3_changes64) orig_sqlite3_changes64 = dlsym(handle, "sqlite3_changes64");
    if (!orig_sqlite3_last_insert_rowid) orig_sqlite3_last_insert_rowid = dlsym(handle, "sqlite3_last_insert_rowid");

    // Error
    if (!orig_sqlite3_errmsg) orig_sqlite3_errmsg = dlsym(handle, "sqlite3_errmsg");
    if (!orig_sqlite3_errcode) orig_sqlite3_errcode = dlsym(handle, "sqlite3_errcode");
    if (!orig_sqlite3_extended_errcode) orig_sqlite3_extended_errcode = dlsym(handle, "sqlite3_extended_errcode");

    // Prepare
    if (!orig_sqlite3_prepare) orig_sqlite3_prepare = dlsym(handle, "sqlite3_prepare");
    if (!orig_sqlite3_prepare_v2) orig_sqlite3_prepare_v2 = dlsym(handle, "sqlite3_prepare_v2");
    if (!orig_sqlite3_prepare_v3) orig_sqlite3_prepare_v3 = dlsym(handle, "sqlite3_prepare_v3");
    if (!orig_sqlite3_prepare16_v2) orig_sqlite3_prepare16_v2 = dlsym(handle, "sqlite3_prepare16_v2");

    // Bind
    if (!orig_sqlite3_bind_int) orig_sqlite3_bind_int = dlsym(handle, "sqlite3_bind_int");
    if (!orig_sqlite3_bind_int64) orig_sqlite3_bind_int64 = dlsym(handle, "sqlite3_bind_int64");
    if (!orig_sqlite3_bind_double) orig_sqlite3_bind_double = dlsym(handle, "sqlite3_bind_double");
    if (!orig_sqlite3_bind_text) orig_sqlite3_bind_text = dlsym(handle, "sqlite3_bind_text");
    if (!orig_sqlite3_bind_text64) orig_sqlite3_bind_text64 = dlsym(handle, "sqlite3_bind_text64");
    if (!orig_sqlite3_bind_blob) orig_sqlite3_bind_blob = dlsym(handle, "sqlite3_bind_blob");
    if (!orig_sqlite3_bind_blob64) orig_sqlite3_bind_blob64 = dlsym(handle, "sqlite3_bind_blob64");
    if (!orig_sqlite3_bind_value) orig_sqlite3_bind_value = dlsym(handle, "sqlite3_bind_value");
    if (!orig_sqlite3_bind_null) orig_sqlite3_bind_null = dlsym(handle, "sqlite3_bind_null");

    // Step/Reset/Finalize
    if (!orig_sqlite3_step) orig_sqlite3_step = dlsym(handle, "sqlite3_step");
    if (!orig_sqlite3_reset) orig_sqlite3_reset = dlsym(handle, "sqlite3_reset");
    if (!orig_sqlite3_finalize) orig_sqlite3_finalize = dlsym(handle, "sqlite3_finalize");
    if (!orig_sqlite3_clear_bindings) orig_sqlite3_clear_bindings = dlsym(handle, "sqlite3_clear_bindings");

    // Column access
    if (!orig_sqlite3_column_count) orig_sqlite3_column_count = dlsym(handle, "sqlite3_column_count");
    if (!orig_sqlite3_column_type) orig_sqlite3_column_type = dlsym(handle, "sqlite3_column_type");
    if (!orig_sqlite3_column_int) orig_sqlite3_column_int = dlsym(handle, "sqlite3_column_int");
    if (!orig_sqlite3_column_int64) orig_sqlite3_column_int64 = dlsym(handle, "sqlite3_column_int64");
    if (!orig_sqlite3_column_double) orig_sqlite3_column_double = dlsym(handle, "sqlite3_column_double");
    if (!orig_sqlite3_column_text) orig_sqlite3_column_text = dlsym(handle, "sqlite3_column_text");
    if (!orig_sqlite3_column_blob) orig_sqlite3_column_blob = dlsym(handle, "sqlite3_column_blob");
    if (!orig_sqlite3_column_bytes) orig_sqlite3_column_bytes = dlsym(handle, "sqlite3_column_bytes");
    if (!orig_sqlite3_column_name) orig_sqlite3_column_name = dlsym(handle, "sqlite3_column_name");
    if (!orig_sqlite3_column_decltype) orig_sqlite3_column_decltype = dlsym(handle, "sqlite3_column_decltype");
    if (!orig_sqlite3_column_value) orig_sqlite3_column_value = dlsym(handle, "sqlite3_column_value");
    if (!orig_sqlite3_data_count) orig_sqlite3_data_count = dlsym(handle, "sqlite3_data_count");

    // Value access
    if (!orig_sqlite3_value_type) orig_sqlite3_value_type = dlsym(handle, "sqlite3_value_type");
    if (!orig_sqlite3_value_text) orig_sqlite3_value_text = dlsym(handle, "sqlite3_value_text");
    if (!orig_sqlite3_value_int) orig_sqlite3_value_int = dlsym(handle, "sqlite3_value_int");
    if (!orig_sqlite3_value_int64) orig_sqlite3_value_int64 = dlsym(handle, "sqlite3_value_int64");
    if (!orig_sqlite3_value_double) orig_sqlite3_value_double = dlsym(handle, "sqlite3_value_double");
    if (!orig_sqlite3_value_bytes) orig_sqlite3_value_bytes = dlsym(handle, "sqlite3_value_bytes");
    if (!orig_sqlite3_value_blob) orig_sqlite3_value_blob = dlsym(handle, "sqlite3_value_blob");

    // Collation
    if (!orig_sqlite3_create_collation) orig_sqlite3_create_collation = dlsym(handle, "sqlite3_create_collation");
    if (!orig_sqlite3_create_collation_v2) orig_sqlite3_create_collation_v2 = dlsym(handle, "sqlite3_create_collation_v2");

    // Memory, statement info, misc
    if (!orig_sqlite3_free) orig_sqlite3_free = dlsym(handle, "sqlite3_free");
    if (!orig_sqlite3_malloc) orig_sqlite3_malloc = dlsym(handle, "sqlite3_malloc");
    if (!orig_sqlite3_db_handle) orig_sqlite3_db_handle = dlsym(handle, "sqlite3_db_handle");
    if (!orig_sqlite3_sql) orig_sqlite3_sql = dlsym(handle, "sqlite3_sql");
    if (!orig_sqlite3_expanded_sql) orig_sqlite3_expanded_sql = dlsym(handle, "sqlite3_expanded_sql");
    if (!orig_sqlite3_bind_parameter_count) orig_sqlite3_bind_parameter_count = dlsym(handle, "sqlite3_bind_parameter_count");
    if (!orig_sqlite3_bind_parameter_index) orig_sqlite3_bind_parameter_index = dlsym(handle, "sqlite3_bind_parameter_index");
    if (!orig_sqlite3_bind_parameter_name) orig_sqlite3_bind_parameter_name = dlsym(handle, "sqlite3_bind_parameter_name");
    if (!orig_sqlite3_stmt_readonly) orig_sqlite3_stmt_readonly = dlsym(handle, "sqlite3_stmt_readonly");
    if (!orig_sqlite3_stmt_busy) orig_sqlite3_stmt_busy = dlsym(handle, "sqlite3_stmt_busy");
    if (!orig_sqlite3_stmt_status) orig_sqlite3_stmt_status = dlsym(handle, "sqlite3_stmt_status");

    // Set up aliases for backward compatibility
    if (!real_sqlite3_prepare_v2) real_sqlite3_prepare_v2 = orig_sqlite3_prepare_v2;
    if (!real_sqlite3_errmsg) real_sqlite3_errmsg = orig_sqlite3_errmsg;
    if (!real_sqlite3_errcode) real_sqlite3_errcode = orig_sqlite3_errcode;

    // Log critical symbol resolution
    if (orig_sqlite3_open) {
        fprintf(stderr, "[SHIM_INIT] orig_sqlite3_open = %p\n", (void*)orig_sqlite3_open);
    } else {
        fprintf(stderr, "[SHIM_INIT] WARNING: orig_sqlite3_open is NULL!\n");
    }
    if (orig_sqlite3_prepare_v2) {
        fprintf(stderr, "[SHIM_INIT] orig_sqlite3_prepare_v2 = %p\n", (void*)orig_sqlite3_prepare_v2);
    } else {
        fprintf(stderr, "[SHIM_INIT] WARNING: orig_sqlite3_prepare_v2 is NULL!\n");
    }
}

// ============================================================================
// Symbol Resolution Safety Check
// ============================================================================

static volatile int symbols_verified = 0;

int shim_ensure_ready(void) {
    // Fast path: already verified
    if (symbols_verified) return 1;
    
    // Memory barrier to ensure we see latest values
    __sync_synchronize();
    
    // Check if constructor has completed
    if (!shim_initialized) {
        fprintf(stderr, "[SHIM] WARNING: shim_ensure_ready called before shim_initialized!\n");
        fflush(stderr);
        return 0;
    }
    
    // Verify critical function pointers
    if (!orig_sqlite3_open || !orig_sqlite3_prepare_v2 || !orig_sqlite3_step) {
        fprintf(stderr, "[SHIM] WARNING: Critical symbols NULL, attempting fallback...\n");
        fflush(stderr);
        
#ifdef __APPLE__
        // macOS: use sqlite_handle from dlopen
        if (sqlite_handle) {
            if (!orig_sqlite3_open) 
                orig_sqlite3_open = dlsym(sqlite_handle, "sqlite3_open");
            if (!orig_sqlite3_prepare_v2) 
                orig_sqlite3_prepare_v2 = dlsym(sqlite_handle, "sqlite3_prepare_v2");
            if (!orig_sqlite3_step) 
                orig_sqlite3_step = dlsym(sqlite_handle, "sqlite3_step");
        }
#else
        // Linux: use RTLD_NEXT
        if (!orig_sqlite3_open) 
            orig_sqlite3_open = dlsym(RTLD_NEXT, "sqlite3_open");
        if (!orig_sqlite3_prepare_v2) 
            orig_sqlite3_prepare_v2 = dlsym(RTLD_NEXT, "sqlite3_prepare_v2");
        if (!orig_sqlite3_step) 
            orig_sqlite3_step = dlsym(RTLD_NEXT, "sqlite3_step");
#endif
        
        // Check again
        if (!orig_sqlite3_open || !orig_sqlite3_prepare_v2 || !orig_sqlite3_step) {
            fprintf(stderr, "[SHIM] FATAL: Cannot resolve critical SQLite symbols!\n");
            fflush(stderr);
            return 0;
        }
    }
    
    // All checks passed - mark as verified
    symbols_verified = 1;
    return 1;
}

void reset_symbol_verification(void) {
    symbols_verified = 0;
}

// ============================================================================
// Worker Thread Implementation
// ============================================================================

static void* worker_thread_func(void *arg) {
    (void)arg;
    LOG_INFO("WORKER: Thread started with %d MB stack", WORKER_STACK_SIZE / (1024*1024));

    while (1) {
        pthread_mutex_lock(&worker_mutex);

        // Wait for work
        while (!worker_request.work_ready && worker_running) {
            pthread_cond_wait(&worker_cond_request, &worker_mutex);
        }

        if (!worker_running) {
            pthread_mutex_unlock(&worker_mutex);
            break;
        }

        worker_request.work_ready = 0;

        // Handle the request
        if (worker_request.type == WORK_SHUTDOWN) {
            worker_request.work_done = 1;
            pthread_cond_signal(&worker_cond_response);
            pthread_mutex_unlock(&worker_mutex);
            break;
        }

        if (worker_request.type == WORK_PREPARE_V2) {
            sqlite3_stmt *stmt = NULL;
            const char *tail = NULL;

            // Call internal prepare with from_worker=1 to avoid recursion
            int rc = my_sqlite3_prepare_v2_internal(
                worker_request.db,
                worker_request.zSql,
                worker_request.nByte,
                &stmt,
                &tail,
                1  // from_worker - prevents re-delegation
            );

            worker_request.stmt = stmt;
            worker_request.tail = tail;
            worker_request.result = rc;
        }

        worker_request.work_done = 1;
        pthread_cond_signal(&worker_cond_response);
        pthread_mutex_unlock(&worker_mutex);
    }

    LOG_INFO("WORKER: Thread exiting");
    return NULL;
}

int worker_init(void) {
    pthread_attr_t attr;
    if (pthread_attr_init(&attr) != 0) {
        LOG_ERROR("WORKER: Failed to init thread attributes");
        return -1;
    }

    // Set 8MB stack size
    if (pthread_attr_setstacksize(&attr, WORKER_STACK_SIZE) != 0) {
        LOG_ERROR("WORKER: Failed to set stack size");
        pthread_attr_destroy(&attr);
        return -1;
    }

    worker_running = 1;
    memset(&worker_request, 0, sizeof(worker_request));

    if (pthread_create(&worker_thread, &attr, worker_thread_func, NULL) != 0) {
        LOG_ERROR("WORKER: Failed to create thread");
        worker_running = 0;
        pthread_attr_destroy(&attr);
        return -1;
    }

    pthread_attr_destroy(&attr);
    LOG_INFO("WORKER: Initialized with %d MB stack", WORKER_STACK_SIZE / (1024*1024));
    return 0;
}

void worker_cleanup(void) {
    if (!worker_running) return;

    pthread_mutex_lock(&worker_mutex);
    worker_request.type = WORK_SHUTDOWN;
    worker_request.work_ready = 1;
    worker_running = 0;
    pthread_cond_signal(&worker_cond_request);
    pthread_mutex_unlock(&worker_mutex);

    pthread_join(worker_thread, NULL);
    LOG_INFO("WORKER: Cleaned up");
}

// Delegate prepare_v2 to worker thread (called when stack is low)
int delegate_prepare_to_worker(sqlite3 *db, const char *zSql, int nByte,
                               sqlite3_stmt **ppStmt, const char **pzTail) {
    if (!worker_running) {
        LOG_ERROR("WORKER: Not running, cannot delegate");
        return SQLITE_ERROR;
    }

    LOG_DEBUG("WORKER: Delegating query (%.100s)", zSql ? zSql : "NULL");

    pthread_mutex_lock(&worker_mutex);

    // Set up request
    worker_request.type = WORK_PREPARE_V2;
    worker_request.db = db;
    worker_request.zSql = zSql;
    worker_request.nByte = nByte;
    worker_request.stmt = NULL;
    worker_request.tail = NULL;
    worker_request.result = SQLITE_ERROR;
    worker_request.work_done = 0;
    worker_request.work_ready = 1;

    // Signal worker
    pthread_cond_signal(&worker_cond_request);

    // Wait for response
    while (!worker_request.work_done) {
        pthread_cond_wait(&worker_cond_response, &worker_mutex);
    }

    // Get results
    if (ppStmt) *ppStmt = worker_request.stmt;
    if (pzTail) *pzTail = worker_request.tail;
    int result = worker_request.result;

    pthread_mutex_unlock(&worker_mutex);

    LOG_DEBUG("WORKER: Delegation complete, rc=%d", result);
    return result;
}

// ============================================================================
// Fork Handlers - Critical for Connection Pool Safety
// ============================================================================

// Called in PARENT before fork()
void common_atfork_prepare(void) {
    // No action needed - parent continues with its connections
}

// Called in PARENT after fork()
void common_atfork_parent(void) {
    // No action needed - parent keeps its connections
}

// Called in CHILD after fork()
void common_atfork_child(void) {
    // CRITICAL: Child process must NOT use parent's PostgreSQL connections
    // The PostgreSQL protocol is not fork-safe - sockets are in the middle of I/O

    // Use fprintf since logging may not be initialized yet
    fprintf(stderr, "[FORK_CHILD] Cleaning up inherited connection pool (child PID %d)\n", getpid());
    fflush(stderr);

    // Clear exception context - parent's pointers are not valid in child
    last_query_being_processed = NULL;
    last_column_being_accessed = NULL;
    global_value_type_calls = 0;
    global_column_type_calls = 0;

    // Reset exception tracking for child process
    reset_exception_tracking();

    // Reset symbol verification - child needs to re-verify
    reset_symbol_verification();

    // Call pg_client cleanup function to clear pool state
    extern void pg_pool_cleanup_after_fork(void);
    pg_pool_cleanup_after_fork();

    // Reset logging to prevent mutex deadlock
    extern void pg_logging_reset_after_fork(void);
    pg_logging_reset_after_fork();

    fprintf(stderr, "[FORK_CHILD] Pool and logging reset, child will reinitialize\n");
    fflush(stderr);
}

// ============================================================================
// Common Initialization
// ============================================================================

// Check if we're in a forked process and reset state if needed
int common_check_fork(void) {
    pid_t current_pid = getpid();
    
    if (shim_init_pid != 0 && shim_init_pid != current_pid) {
        fprintf(stderr, "[SHIM_INIT] Detected fork (parent PID %d, our PID %d) - resetting state\n",
                shim_init_pid, current_pid);
        fflush(stderr);
        
        shim_initialized = 0;
        last_query_being_processed = NULL;
        last_column_being_accessed = NULL;
        global_value_type_calls = 0;
        global_column_type_calls = 0;
        reset_exception_tracking();
        
        shim_init_pid = current_pid;
        return 1;  // Fork detected
    }
    
    shim_init_pid = current_pid;
    return 0;  // No fork
}

void common_shim_init_modules(void) {
    pg_config_init();
    pg_client_init();
    pg_statement_init();
    pg_query_cache_init();
    sql_translator_init();
    worker_init();
}

void common_shim_cleanup(void) {
    worker_cleanup();
    pg_statement_cleanup();
    pg_client_cleanup();
    sql_translator_cleanup();
    pg_logging_cleanup();
}

// ============================================================================
// Common Signal Handler
// ============================================================================

void common_signal_handler(int sig) {
    const char *sig_name = "UNKNOWN";
    const char *sig_desc = "Unknown signal";
    
    switch(sig) {
        case SIGSEGV: sig_name = "SIGSEGV"; sig_desc = "Segmentation fault"; break;
#ifdef SIGBUS
        case SIGBUS:  sig_name = "SIGBUS";  sig_desc = "Bus error"; break;
#endif
        case SIGFPE:  sig_name = "SIGFPE";  sig_desc = "Floating point exception"; break;
        case SIGILL:  sig_name = "SIGILL";  sig_desc = "Illegal instruction"; break;
        case SIGABRT: sig_name = "SIGABRT"; sig_desc = "Abort"; break;
    }
    
    // Print shim context (useful for debugging)
    const char *ctx_query = last_query_being_processed;
    const char *ctx_column = last_column_being_accessed;
    
    fprintf(stderr, "\n");
    fprintf(stderr, "╔══════════════════════════════════════════════════════════════════════════════╗\n");
    fprintf(stderr, "║ FATAL SIGNAL: %-64s ║\n", sig_name);
    fprintf(stderr, "║ Description:  %-64s ║\n", sig_desc);
    fprintf(stderr, "╠══════════════════════════════════════════════════════════════════════════════╣\n");
    
    if (ctx_query) {
        char q[65];
        snprintf(q, sizeof(q), "%.64s", ctx_query);
        fprintf(stderr, "║ Last Query:  %-65s ║\n", q);
    }
    if (ctx_column) {
        fprintf(stderr, "║ Last Column: %-65s ║\n", ctx_column);
    }
    
    fprintf(stderr, "╚══════════════════════════════════════════════════════════════════════════════╝\n");
    
    // Print platform-specific backtrace
    platform_print_backtrace(sig_name, 1);
    
    LOG_ERROR("FATAL SIGNAL: %s (%s)", sig_name, sig_desc);
    
    // Re-raise signal with default handler
    signal(sig, SIG_DFL);
    raise(sig);
}

// ============================================================================
// Common Exception Info Printing
// ============================================================================

char* print_exception_info(const char *type_name, int count) {
    // Initialize demangle function if needed
    if (!cxa_demangle_fn) {
        cxa_demangle_fn = (char* (*)(const char*, char*, size_t*, int*))dlsym(RTLD_DEFAULT, "__cxa_demangle");
    }
    
    // Demangle type name
    char *demangled = NULL;
    if (cxa_demangle_fn && type_name) {
        int status = 0;
        demangled = cxa_demangle_fn(type_name, NULL, NULL, &status);
    }
    const char *readable_name = demangled ? demangled : (type_name ? type_name : "unknown");
    
    // Get shim context
    const char *ctx_query = last_query_being_processed;
    const char *ctx_column = last_column_being_accessed;
    long ctx_value_calls = global_value_type_calls;
    long ctx_column_calls = global_column_type_calls;
    int is_shim_related = (ctx_value_calls > 0 || ctx_column_calls > 0 || ctx_query != NULL);
    int tls_is_shim_related = (tls_column_type_calls > 0 || tls_value_type_calls > 0 || tls_last_query != NULL);
    
    pthread_t tid = pthread_self();
    
    fprintf(stderr, "\n");
    fprintf(stderr, "╔══════════════════════════════════════════════════════════════════════════════╗\n");
    fprintf(stderr, "║ C++ EXCEPTION #%-4d                                                          ║\n", count);
    fprintf(stderr, "╠══════════════════════════════════════════════════════════════════════════════╣\n");
    
    // Truncate long type names
    char type_display[73];
    snprintf(type_display, sizeof(type_display), "%.72s", readable_name);
    fprintf(stderr, "║ Type: %-72s ║\n", type_display);
    fprintf(stderr, "║ PID: %-6d  Thread: 0x%-54lx ║\n", getpid(), (unsigned long)tid);
    
    fprintf(stderr, "╠══════════════════════════════════════════════════════════════════════════════╣\n");
    
    if (is_shim_related) {
        fprintf(stderr, "║ SHIM STATE:                                                                  ║\n");
        fprintf(stderr, "║   Global: col_type=%-5ld val_type=%-5ld                                      ║\n",
                ctx_column_calls, ctx_value_calls);
        fprintf(stderr, "║   Thread: col_type=%-5ld val_type=%-5ld (this_thread_used_shim=%s)           ║\n",
                tls_column_type_calls, tls_value_type_calls, tls_is_shim_related ? "YES" : "NO ");
        if (!tls_is_shim_related) {
            fprintf(stderr, "║   NOTE: This thread has NOT made any SQLite calls through shim!             ║\n");
        }
        if (ctx_query && ctx_query[0]) {
            char query_snippet[55];
            snprintf(query_snippet, sizeof(query_snippet), "%.54s", ctx_query);
            fprintf(stderr, "║   Last Query (any thread): %-51s ║\n", query_snippet);
        }
        if (ctx_column && ctx_column[0]) {
            fprintf(stderr, "║   Last Column: %-63s ║\n", ctx_column);
        }
    } else {
        fprintf(stderr, "║ NOT SHIM-RELATED: No SQLite calls have been made through the shim            ║\n");
    }
    
    LOG_ERROR("EXCEPTION #%d [%s]: shim=%s tls_shim=%s col=%ld val=%ld",
              count, readable_name,
              is_shim_related ? "YES" : "NO",
              tls_is_shim_related ? "YES" : "NO",
              ctx_column_calls, ctx_value_calls);
    
    // Return demangled name - caller is responsible for freeing it
    return demangled;
}

// ============================================================================
// Common Exception Handler Logic
// ============================================================================

int common_handle_exception(void *thrown_exception, void *tinfo,
                           int *in_handler_flag,
                           int *should_call_original) {
    (void)thrown_exception;
    
    *should_call_original = 1;  // Default: always call original
    
    // Prevent recursion
    if (*in_handler_flag) {
        return 0;  // Recursion detected, caller should call original and abort
    }
    *in_handler_flag = 1;
    
    int total_count = __sync_add_and_fetch(&total_exception_count, 1);
    const char *type_name = get_type_name(tinfo);
    exception_type_tracker_t *tracker = get_exception_tracker(type_name);
    
    // Determine if we should log this exception
    int is_db_exception = (type_name && (strstr(type_name, "DB") || strstr(type_name, "Exception") || 
                           strstr(type_name, "exception") || strstr(type_name, "Error")));
    
    int should_log = is_db_exception || 
                     ((total_count <= MAX_LOGGED_TOTAL) &&
                      (tracker == NULL || tracker->count <= MAX_LOGGED_PER_TYPE));
    int should_trace = is_db_exception || (tracker && !tracker->logged_with_trace);
    
    if (should_log) {
        // Print exception info
        char *demangled = print_exception_info(type_name, total_count);
        
        if (should_trace) {
            if (tracker) tracker->logged_with_trace = 1;
            platform_print_backtrace("Exception Stack Trace", 2);
        }
        
        fprintf(stderr, "╚══════════════════════════════════════════════════════════════════════════════╝\n");
        fflush(stderr);
        
        if (demangled) free(demangled);
    } else if (total_count == MAX_LOGGED_TOTAL + 1) {
        fprintf(stderr, "\n╔══════════════════════════════════════════════════════════════════════════════╗\n");
        fprintf(stderr, "║ [THROTTLE] Exception logging limited (>%d). Summary in log file.              ║\n", MAX_LOGGED_TOTAL);
        fprintf(stderr, "╚══════════════════════════════════════════════════════════════════════════════╝\n");
        fflush(stderr);
    }
    
    *in_handler_flag = 0;
    return 1;  // Success, caller should call original
}
