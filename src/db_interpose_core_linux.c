/*
 * Plex PostgreSQL Interposing Shim - Core Module (Linux)
 *
 * This is the Linux-specific entry point containing:
 * - LD_PRELOAD wrapper functions
 * - Linux-specific backtrace/exception handling
 * - __cxa_throw interception
 * - sigaction() interception (prevents libpq from altering signal handlers)
 * - Constructor/destructor
 *
 * Common code is in db_interpose_common.c
 */

#define _GNU_SOURCE
#include "db_interpose.h"
#include "db_interpose_common.h"
#include "pg_query_cache.h"
#include <signal.h>
#include <dlfcn.h>

// ============================================================================
// sigaction() Interception (SIGCHLD guard)
// ============================================================================
//
// Plex crashes with "Received unexpected async signal 17" when child processes
// exit under LD_PRELOAD. To prevent this, we force SIGCHLD to SIG_IGN in the
// main Plex Server/Scanner processes. This makes child exit notifications
// auto-reaped by kernel and avoids Plex's fragile async signal path.

static int (*orig_sigaction)(int, const struct sigaction *, struct sigaction *) = NULL;
static volatile int force_ignore_sigchld = 0;

int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact) {
    if (__builtin_expect(!orig_sigaction, 0)) {
        orig_sigaction = dlsym(RTLD_NEXT, "sigaction");
        if (!orig_sigaction) return -1;
    }

    if (force_ignore_sigchld && signum == SIGCHLD && act != NULL) {
        if (oldact) orig_sigaction(SIGCHLD, NULL, oldact);
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_handler = SIG_IGN;
        sigemptyset(&sa.sa_mask);
        sa.sa_flags = SA_NOCLDSTOP;
        return orig_sigaction(SIGCHLD, &sa, NULL);
    }

    return orig_sigaction(signum, act, oldact);
}

// platform_print_backtrace is in platform_backtrace.c

// ============================================================================
// C++ Exception Interception (Linux via direct override)
// ============================================================================
//
// IMPORTANT: The __cxa_throw override is DISABLED by default on Linux.
// Even a trivial passthrough override causes Plex to crash on aarch64/musl
// because the C-compiled wrapper interferes with C++ exception unwinding.
//
// To enable at runtime, set PLEX_PG_EXCEPTION_LOG=1 AND rebuild with:
//   -DENABLE_CXA_THROW_OVERRIDE
//
// On macOS, the equivalent functionality uses Objective-C interposing which
// doesn't have this issue.

#ifdef ENABLE_CXA_THROW_OVERRIDE
// Original __cxa_throw function pointer
static void (*orig_cxa_throw)(void*, void*, void(*)(void*)) = NULL;

// Thread-local recursion prevention
static __thread int in_exception_handler = 0;

// Whether __cxa_throw interception is enabled at runtime
static int exception_log_enabled = 0;
#endif

// Thread-local counters and demangle function are in db_interpose_common.c

// ============================================================================
// __cxa_throw Override (Linux)
// ============================================================================

#ifdef ENABLE_CXA_THROW_OVERRIDE
__attribute__((noreturn))
void __cxa_throw(void *thrown_exception, void *tinfo, void (*dest)(void*)) {
    // Load original on first call
    if (__builtin_expect(!orig_cxa_throw, 0)) {
        orig_cxa_throw = (void (*)(void*, void*, void(*)(void*)))dlsym(RTLD_NEXT, "__cxa_throw");
    }

    // When exception logging is disabled at runtime, pass through immediately
    if (__builtin_expect(!exception_log_enabled, 1)) {
        if (orig_cxa_throw) {
            orig_cxa_throw(thrown_exception, tinfo, dest);
        }
        __builtin_unreachable();
    }

    int should_call_original = 1;
    
    // Use common exception handling logic
    if (!common_handle_exception(thrown_exception, tinfo, &in_exception_handler, &should_call_original)) {
        // Recursion detected
        if (orig_cxa_throw) {
            orig_cxa_throw(thrown_exception, tinfo, dest);
        }
        __builtin_unreachable();
    }
    
    // Call original
    if (orig_cxa_throw) {
        orig_cxa_throw(thrown_exception, tinfo, dest);
    }

    __builtin_unreachable();
}
#endif /* ENABLE_CXA_THROW_OVERRIDE */

// Signal handler uses common implementation from db_interpose_common.c

// ============================================================================
// Load Original SQLite Functions (Linux via RTLD_NEXT)
// ============================================================================

static void *real_sqlite_handle = NULL;

static void load_original_functions(void) {
    const char *sqlite_paths[] = {
        "/usr/local/lib/plex-postgresql/libsqlite3_real.so",
        "/usr/lib/plexmediaserver/lib/libsqlite3.so.original",
        "/usr/lib/plexmediaserver/lib/libsqlite3.so",
        NULL
    };

    void *handle = NULL;
    for (int i = 0; sqlite_paths[i] != NULL; i++) {
        handle = dlopen(sqlite_paths[i], RTLD_NOW | RTLD_LOCAL);
        if (handle) {
            fprintf(stderr, "[SHIM_INIT] Loaded real SQLite from %s\n", sqlite_paths[i]);
            real_sqlite_handle = handle;
            break;
        }
    }

    if (!handle) {
        fprintf(stderr, "[SHIM_INIT] Loading original SQLite functions via RTLD_NEXT...\n");
        handle = RTLD_NEXT;
    }

    // Load all function pointers via shared helper (includes verification logging)
    common_load_sqlite_symbols(handle);

    fprintf(stderr, "[SHIM_INIT] Original SQLite functions loaded\n");
}

void ensure_real_sqlite_loaded(void) {
    if (real_sqlite3_prepare_v2) return;
    real_sqlite3_prepare_v2 = orig_sqlite3_prepare_v2;
    real_sqlite3_errmsg = orig_sqlite3_errmsg;
    real_sqlite3_errcode = orig_sqlite3_errcode;
}

// ============================================================================
// Constructor/Destructor (Linux)
// ============================================================================

// shim_init_pid is in db_interpose_common.c

__attribute__((constructor))
static void shim_init(void) {
    fprintf(stderr, "[SHIM_INIT] Constructor starting (Linux)...\n");
    fflush(stderr);

#ifdef ENABLE_CXA_THROW_OVERRIDE
    // Check if exception logging is enabled at runtime (only relevant when compiled with override)
    // Set PLEX_PG_EXCEPTION_LOG=1 to enable diagnostic C++ exception interception
    {
        const char *exc_log = getenv(ENV_PG_EXCEPTION_LOG);
        if (exc_log && (exc_log[0] == '1' || exc_log[0] == 'y' || exc_log[0] == 'Y')) {
            exception_log_enabled = 1;
            fprintf(stderr, "[SHIM_INIT] C++ exception logging ENABLED via %s\n", ENV_PG_EXCEPTION_LOG);
        }
    }
#endif

    // On Linux, LD_PRELOAD is inherited by ALL child processes (plugins, CrashUploader, etc.)
    // Unlike macOS where DYLD_INSERT_LIBRARIES is stripped by Sequoia at every execv.
    // Only fully initialize the shim for "Plex Media Server" and "Plex Media Scanner".
    // Other processes (Python plugins, CrashUploader) must be completely skipped —
    // no fork handlers, no signal handlers, no SQLite loading, nothing.
    {
        char proc_name[256] = {0};
        FILE *cmdline = fopen("/proc/self/cmdline", "r");
        if (cmdline) {
            size_t n = fread(proc_name, 1, sizeof(proc_name) - 1, cmdline);
            fclose(cmdline);
            const char *base = proc_name;
            for (size_t i = 0; i < n && proc_name[i]; i++) {
                if (proc_name[i] == '/') base = &proc_name[i + 1];
            }
            if (strstr(base, "Plex Media Server") == NULL &&
                strstr(base, "Plex Media Scanner") == NULL) {
                // Non-target process: stay in passthrough mode.
                // We still must resolve original SQLite symbols so any sqlite3_*
                // calls from this process (plugins, helpers) continue to work.
                force_ignore_sigchld = 0;
                shim_passthrough_only = 1;
                load_original_functions();
                shim_initialized = 1;
                fprintf(stderr, "[SHIM_INIT] Not Plex Server/Scanner ('%s'), skipping entirely (PID %d)\n",
                        base, getpid());
                fflush(stderr);
                return;
            }

            force_ignore_sigchld = 1;
        }
    }

    // Detect fork and reset state if needed
    common_check_fork();

    // NOTE: We intentionally do NOT register pthread_atfork on Linux.
    // On Linux, LD_PRELOAD is inherited by all child processes. When Plex forks
    // to spawn CrashUploader/plugins, the atfork child handler would run in the
    // child before exec(). This disrupts Plex's signal handling in the parent,
    // causing "Received unexpected async signal 17" (SIGCHLD) crashes.
    //
    // Instead, we rely on:
    // 1. Process name check above (skips non-Server/Scanner after exec)
    // 2. common_check_fork() PID detection (handles same-binary forks)
    // 3. The constructor re-running after exec() in child processes
    //
    // On macOS, DYLD_INSERT_LIBRARIES is stripped at every execv by Sequoia,
    // so this issue doesn't apply there.
    fprintf(stderr, "[SHIM_INIT] Fork safety: using PID-based detection (no pthread_atfork)\n");
    fflush(stderr);

    // Load original SQLite functions
    load_original_functions();

    // Skip full initialization if SQLite isn't loaded
    if (!orig_sqlite3_open || !orig_sqlite3_prepare_v2) {
        fprintf(stderr, "[SHIM_INIT] SQLite not found in this process, skipping initialization\n");
        fflush(stderr);
        return;
    }

    pg_logging_init();
    LOG_INFO("=== Plex PostgreSQL Interpose Shim loaded (Linux) ===");

    fprintf(stderr, "[SHIM_INIT] Logging initialized\n");
    fflush(stderr);

    // Initialize common modules (pg_client, statement cache, query cache, etc.)
    // libpq's PQconnectdb() may call pqsignal() to set SIGPIPE=SIG_IGN, which
    // is fine — we want SIGPIPE ignored for socket I/O.
    common_shim_init_modules();

    // Keep SIGCHLD ignored in Plex main process to avoid async signal 17 crashes.
    if (force_ignore_sigchld && orig_sigaction) {
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_handler = SIG_IGN;
        sigemptyset(&sa.sa_mask);
        sa.sa_flags = SA_NOCLDSTOP;
        orig_sigaction(SIGCHLD, &sa, NULL);
        fprintf(stderr, "[SHIM_INIT] SIGCHLD forced to SIG_IGN (PID %d)\n", getpid());
        fflush(stderr);
    }

    // Save and restore signal state around init to prevent libpq from
    // interfering with Plex's signal setup.
    // (libpq only sets SIGPIPE, which Plex also sets to SIG_IGN, so this is
    // mostly defensive.)

    shim_initialized = 1;

    // Init delay for symbol resolution race condition
    const char *no_delay = getenv("PLEX_PG_NO_INIT_DELAY");
    if (no_delay && (no_delay[0] == '1' || no_delay[0] == 'y' || no_delay[0] == 'Y')) {
        fprintf(stderr, "[SHIM_INIT] Init delay DISABLED via PLEX_PG_NO_INIT_DELAY\n");
        fflush(stderr);
    } else {
        const char *delay_str = getenv("PLEX_PG_INIT_DELAY_MS");
        int delay_ms = delay_str ? atoi(delay_str) : 200;
        
        if (delay_ms > 0) {
            fprintf(stderr, "[SHIM_INIT] Waiting %d ms for symbol resolution (PID %d)...\n", 
                    delay_ms, getpid());
            fflush(stderr);
            __sync_synchronize();
            usleep(delay_ms * 1000);
            __sync_synchronize();
        }
    }

    fprintf(stderr, "[SHIM_INIT] Constructor complete (Linux, PID %d)\n", getpid());
    fflush(stderr);
}

__attribute__((destructor))
static void shim_cleanup(void) {
    if (!shim_initialized) return;

    LOG_INFO("=== Plex PostgreSQL Interpose Shim unloading (Linux) ===");
    common_shim_cleanup();
}

// ============================================================================
// LD_PRELOAD Wrapper Functions (Linux-specific)
// ============================================================================
// 
// These wrappers intercept SQLite calls via LD_PRELOAD and forward to my_* 
// implementations. Using X-macros for simple signatures, manual for complex ones.

// --- Simple wrappers: single db/stmt/value argument ---
#define WRAP_DB_VOID(name) \
    int sqlite3_##name(sqlite3 *db) { return my_sqlite3_##name(db); }
#define WRAP_DB_RET(ret, name) \
    ret sqlite3_##name(sqlite3 *db) { return my_sqlite3_##name(db); }
#define WRAP_STMT_VOID(name) \
    int sqlite3_##name(sqlite3_stmt *s) { return my_sqlite3_##name(s); }
#define WRAP_STMT_RET(ret, name) \
    ret sqlite3_##name(sqlite3_stmt *s) { return my_sqlite3_##name(s); }
#define WRAP_STMT_IDX(ret, name) \
    ret sqlite3_##name(sqlite3_stmt *s, int i) { return my_sqlite3_##name(s, i); }
#define WRAP_VAL_RET(ret, name) \
    ret sqlite3_##name(sqlite3_value *v) { return my_sqlite3_##name(v); }

// Database functions (sqlite3 *db)
WRAP_DB_VOID(changes)
WRAP_DB_RET(sqlite3_int64, changes64)
WRAP_DB_RET(sqlite3_int64, last_insert_rowid)
WRAP_DB_RET(const char*, errmsg)
WRAP_DB_VOID(errcode)
WRAP_DB_VOID(extended_errcode)

// Statement functions (sqlite3_stmt *s)
WRAP_STMT_VOID(step)
WRAP_STMT_VOID(reset)
WRAP_STMT_VOID(finalize)
WRAP_STMT_VOID(clear_bindings)
WRAP_STMT_VOID(column_count)
WRAP_STMT_VOID(data_count)
WRAP_STMT_VOID(bind_parameter_count)
WRAP_STMT_VOID(stmt_readonly)
WRAP_STMT_VOID(stmt_busy)
WRAP_STMT_RET(sqlite3*, db_handle)
WRAP_STMT_RET(char*, expanded_sql)
WRAP_STMT_RET(const char*, sql)

// Statement + index functions (sqlite3_stmt *s, int idx)
WRAP_STMT_IDX(int, column_type)
WRAP_STMT_IDX(int, column_int)
WRAP_STMT_IDX(sqlite3_int64, column_int64)
WRAP_STMT_IDX(double, column_double)
WRAP_STMT_IDX(const unsigned char*, column_text)
WRAP_STMT_IDX(const void*, column_blob)
WRAP_STMT_IDX(int, column_bytes)
WRAP_STMT_IDX(const char*, column_name)
WRAP_STMT_IDX(sqlite3_value*, column_value)
WRAP_STMT_IDX(const char*, bind_parameter_name)

// Value functions (sqlite3_value *v)
WRAP_VAL_RET(int, value_type)
WRAP_VAL_RET(const unsigned char*, value_text)
WRAP_VAL_RET(int, value_int)
WRAP_VAL_RET(sqlite3_int64, value_int64)
WRAP_VAL_RET(double, value_double)
WRAP_VAL_RET(int, value_bytes)
WRAP_VAL_RET(const void*, value_blob)

#undef WRAP_DB_VOID
#undef WRAP_DB_RET
#undef WRAP_STMT_VOID
#undef WRAP_STMT_RET
#undef WRAP_STMT_IDX
#undef WRAP_VAL_RET

// --- Complex wrappers: multiple arguments, special signatures ---

int sqlite3_open(const char *f, sqlite3 **p) { return my_sqlite3_open(f, p); }
int sqlite3_open_v2(const char *f, sqlite3 **p, int fl, const char *v) { return my_sqlite3_open_v2(f, p, fl, v); }
int sqlite3_close(sqlite3 *db) { return my_sqlite3_close(db); }
int sqlite3_close_v2(sqlite3 *db) { return my_sqlite3_close_v2(db); }

int sqlite3_exec(sqlite3 *db, const char *sql, int (*cb)(void*,int,char**,char**), void *a, char **e) {
    return my_sqlite3_exec(db, sql, cb, a, e);
}
int sqlite3_get_table(sqlite3 *db, const char *sql, char ***r, int *nr, int *nc, char **e) {
    return my_sqlite3_get_table(db, sql, r, nr, nc, e);
}

int sqlite3_prepare(sqlite3 *db, const char *sql, int n, sqlite3_stmt **s, const char **t) {
    return my_sqlite3_prepare(db, sql, n, s, t);
}
int sqlite3_prepare_v2(sqlite3 *db, const char *sql, int n, sqlite3_stmt **s, const char **t) {
    return my_sqlite3_prepare_v2(db, sql, n, s, t);
}
int sqlite3_prepare_v3(sqlite3 *db, const char *sql, int n, unsigned int f, sqlite3_stmt **s, const char **t) {
    return my_sqlite3_prepare_v3(db, sql, n, f, s, t);
}
int sqlite3_prepare16_v2(sqlite3 *db, const void *sql, int n, sqlite3_stmt **s, const void **t) {
    return my_sqlite3_prepare16_v2(db, sql, n, s, t);
}

int sqlite3_bind_int(sqlite3_stmt *s, int i, int v) { return my_sqlite3_bind_int(s, i, v); }
int sqlite3_bind_int64(sqlite3_stmt *s, int i, sqlite3_int64 v) { return my_sqlite3_bind_int64(s, i, v); }
int sqlite3_bind_double(sqlite3_stmt *s, int i, double v) { return my_sqlite3_bind_double(s, i, v); }
int sqlite3_bind_null(sqlite3_stmt *s, int i) { return my_sqlite3_bind_null(s, i); }
int sqlite3_bind_text(sqlite3_stmt *s, int i, const char *v, int n, void (*d)(void*)) {
    return my_sqlite3_bind_text(s, i, v, n, d);
}
int sqlite3_bind_text64(sqlite3_stmt *s, int i, const char *v, sqlite3_uint64 n, void (*d)(void*), unsigned char e) {
    return my_sqlite3_bind_text64(s, i, v, n, d, e);
}
int sqlite3_bind_blob(sqlite3_stmt *s, int i, const void *v, int n, void (*d)(void*)) {
    return my_sqlite3_bind_blob(s, i, v, n, d);
}
int sqlite3_bind_blob64(sqlite3_stmt *s, int i, const void *v, sqlite3_uint64 n, void (*d)(void*)) {
    return my_sqlite3_bind_blob64(s, i, v, n, d);
}
int sqlite3_bind_value(sqlite3_stmt *s, int i, const sqlite3_value *v) { return my_sqlite3_bind_value(s, i, v); }
int sqlite3_bind_parameter_index(sqlite3_stmt *s, const char *n) { return my_sqlite3_bind_parameter_index(s, n); }

int sqlite3_stmt_status(sqlite3_stmt *s, int op, int reset) { return my_sqlite3_stmt_status(s, op, reset); }

void sqlite3_free(void *p) { my_sqlite3_free(p); }
void* sqlite3_malloc(int n) { return my_sqlite3_malloc(n); }

int sqlite3_create_collation(sqlite3 *db, const char *name, int enc, void *arg,
                              int(*cmp)(void*,int,const void*,int,const void*)) {
    return my_sqlite3_create_collation(db, name, enc, arg, cmp);
}
int sqlite3_create_collation_v2(sqlite3 *db, const char *name, int enc, void *arg,
                                 int(*cmp)(void*,int,const void*,int,const void*), void(*dest)(void*)) {
    return my_sqlite3_create_collation_v2(db, name, enc, arg, cmp, dest);
}

const char* sqlite3_column_decltype(sqlite3_stmt *s, int i) {
    return my_sqlite3_column_decltype(s, i);
}
