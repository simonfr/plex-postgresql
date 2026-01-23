/*
 * Plex PostgreSQL Interposing Shim - Core Module (Linux)
 *
 * This is the Linux-specific entry point containing:
 * - LD_PRELOAD wrapper functions
 * - Linux-specific backtrace/exception handling
 * - __cxa_throw interception
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

// execinfo.h is glibc-specific, not available on musl
#ifdef __GLIBC__
#include <execinfo.h>
#define HAS_BACKTRACE 1
#else
#define HAS_BACKTRACE 0
#endif

// ============================================================================
// C++ Exception Interception (Linux via direct override)
// ============================================================================

// Original __cxa_throw function pointer
static void (*orig_cxa_throw)(void*, void*, void(*)(void*)) = NULL;

// Thread-local recursion prevention
static __thread int in_exception_handler = 0;

// Thread-local counters and demangle function are in db_interpose_common.c

// ============================================================================
// Stack Trace Support (Linux)
// ============================================================================

#define MAX_STACK_FRAMES 32
#define MAX_MAPS_ENTRIES 256

typedef struct {
    unsigned long start;
    unsigned long end;
    unsigned long offset;
    char perms[8];
    char path[256];
} map_entry_t;

static map_entry_t memory_map[MAX_MAPS_ENTRIES];
static int memory_map_count = 0;

static const char* parse_hex(const char *s, unsigned long *out) {
    unsigned long val = 0;
    while (*s) {
        char c = *s;
        if (c >= '0' && c <= '9') {
            val = (val << 4) | (c - '0');
        } else if (c >= 'a' && c <= 'f') {
            val = (val << 4) | (c - 'a' + 10);
        } else if (c >= 'A' && c <= 'F') {
            val = (val << 4) | (c - 'A' + 10);
        } else {
            break;
        }
        s++;
    }
    *out = val;
    return s;
}

static const char* skip_ws(const char *s) {
    while (*s == ' ' || *s == '\t') s++;
    return s;
}

static void load_memory_map(void) {
    memory_map_count = 0;

    FILE *maps = fopen("/proc/self/maps", "r");
    if (!maps) return;

    char line[512];
    while (fgets(line, sizeof(line), maps) && memory_map_count < MAX_MAPS_ENTRIES) {
        map_entry_t *e = &memory_map[memory_map_count];
        const char *p = line;

        unsigned long start = 0;
        p = parse_hex(p, &start);
        if (*p != '-') continue;
        p++;

        unsigned long end = 0;
        p = parse_hex(p, &end);
        p = skip_ws(p);

        char perms[8] = {0};
        int pi = 0;
        while (*p && *p != ' ' && pi < 7) {
            perms[pi++] = *p++;
        }
        p = skip_ws(p);

        unsigned long offset = 0;
        p = parse_hex(p, &offset);
        p = skip_ws(p);

        while (*p && *p != ' ') p++;
        p = skip_ws(p);
        while (*p && *p != ' ') p++;
        p = skip_ws(p);

        char path[256] = {0};
        if (*p && *p != '\n') {
            int plen = 0;
            while (*p && *p != '\n' && plen < 255) {
                path[plen++] = *p++;
            }
            path[plen] = '\0';
        }

        e->start = start;
        e->end = end;
        e->offset = offset;
        strncpy(e->perms, perms, sizeof(e->perms) - 1);
        if (path[0]) {
            strncpy(e->path, path, sizeof(e->path) - 1);
        } else {
            strcpy(e->path, "[anonymous]");
        }
        memory_map_count++;
    }

    fclose(maps);
}

static map_entry_t* find_map_entry(unsigned long addr) {
    for (int i = 0; i < memory_map_count; i++) {
        if (addr >= memory_map[i].start && addr < memory_map[i].end) {
            return &memory_map[i];
        }
    }
    return NULL;
}

static const char* get_basename(const char *path) {
    const char *base = strrchr(path, '/');
    return base ? base + 1 : path;
}

static int collect_stack_frames(void **frames, int max_frames) {
    int depth = 0;
    void **fp = __builtin_frame_address(0);
    int iterations = 0;

    while (fp && depth < max_frames && iterations < 100) {
        iterations++;

        if ((unsigned long)fp < 0x1000 || (unsigned long)fp > 0xffffffffffff) break;

        void *ret_addr = NULL;
        #if defined(__aarch64__)
        ret_addr = *((void**)((char*)fp + 8));
        #elif defined(__x86_64__)
        ret_addr = *((void**)((char*)fp + 8));
        #else
        ret_addr = fp[1];
        #endif

        if (!ret_addr || (unsigned long)ret_addr < 0x1000) break;

        frames[depth++] = ret_addr;

        void **next_fp = (void**)*fp;
        if (!next_fp) break;
        if (next_fp <= fp) break;
        if ((unsigned long)next_fp - (unsigned long)fp > 0x100000) break;

        fp = next_fp;
    }

    return depth;
}

// Platform-specific backtrace implementation (Linux uses frame walking + /proc/maps)
void platform_print_backtrace(const char *reason, int skip_frames) {
    void *frames[MAX_STACK_FRAMES];
    int depth = collect_stack_frames(frames, MAX_STACK_FRAMES);

    if (depth == 0) {
        fprintf(stderr, "\n  [Stack trace unavailable]\n");
        return;
    }

    fprintf(stderr, "\n");
    fprintf(stderr, "╔══════════════════════════════════════════════════════════════════════════════╗\n");
    fprintf(stderr, "║ BACKTRACE: %-67s ║\n", reason ? reason : "Unknown");
    fprintf(stderr, "╠══════════════════════════════════════════════════════════════════════════════╣\n");
    LOG_ERROR("=== BACKTRACE (%s) ===", reason ? reason : "Unknown");

    load_memory_map();

    int frames_shown = 0;
    for (int i = skip_frames; i < depth && frames_shown < 20; i++) {
        unsigned long addr = (unsigned long)frames[i];
        char func_name[256] = {0};
        char lib_name[256] = {0};
        char lib_display[30] = {0};

        Dl_info info;
        if (dladdr(frames[i], &info)) {
            if (info.dli_fname) {
                strncpy(lib_name, get_basename(info.dli_fname), sizeof(lib_name) - 1);
            }

            if (info.dli_sname) {
                if (cxa_demangle_fn) {
                    int status = 0;
                    char *demangled = cxa_demangle_fn(info.dli_sname, NULL, NULL, &status);
                    if (demangled) {
                        strncpy(func_name, demangled, sizeof(func_name) - 1);
                        free(demangled);
                    } else {
                        strncpy(func_name, info.dli_sname, sizeof(func_name) - 1);
                    }
                } else {
                    strncpy(func_name, info.dli_sname, sizeof(func_name) - 1);
                }
            }
        }

        if (!lib_name[0]) {
            map_entry_t *entry = find_map_entry(addr);
            if (entry) {
                strncpy(lib_name, get_basename(entry->path), sizeof(lib_name) - 1);
            } else {
                strcpy(lib_name, "[unknown]");
            }
        }

        snprintf(lib_display, sizeof(lib_display), "%.28s", lib_name);

        // Truncate long function names
        if (strlen(func_name) > 45) {
            func_name[42] = '.';
            func_name[43] = '.';
            func_name[44] = '.';
            func_name[45] = '\0';
        }

        char demangled_line[256];
        if (func_name[0]) {
            snprintf(demangled_line, sizeof(demangled_line), "[%2d] %-28s %s", 
                    frames_shown, lib_display, func_name);
        } else {
            snprintf(demangled_line, sizeof(demangled_line), "[%2d] %-28s [%p]", 
                    frames_shown, lib_display, frames[i]);
        }

        fprintf(stderr, "║ %-78s ║\n", demangled_line);
        LOG_ERROR("  %s", demangled_line);
        frames_shown++;
    }

    if (depth > skip_frames + 20) {
        fprintf(stderr, "║ ... and %d more frames                                                         ║\n", depth - skip_frames - 20);
    }

    fprintf(stderr, "╚══════════════════════════════════════════════════════════════════════════════╝\n");
    fflush(stderr);
}

// ============================================================================
// __cxa_throw Override (Linux)
// ============================================================================

void __cxa_throw(void *thrown_exception, void *tinfo, void (*dest)(void*)) {
    int should_call_original = 1;
    
    // Use common exception handling logic
    if (!common_handle_exception(thrown_exception, tinfo, &in_exception_handler, &should_call_original)) {
        // Recursion detected
        if (orig_cxa_throw) {
            orig_cxa_throw(thrown_exception, tinfo, dest);
        }
        abort();
    }
    
    // Load original if needed (Linux-specific: via RTLD_NEXT)
    if (!orig_cxa_throw) {
        orig_cxa_throw = (void (*)(void*, void*, void(*)(void*)))dlsym(RTLD_NEXT, "__cxa_throw");
    }
    
    // Call original
    if (orig_cxa_throw) {
        orig_cxa_throw(thrown_exception, tinfo, dest);
    }

    abort();
}

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

    // Load all function pointers
    orig_sqlite3_open = dlsym(handle, "sqlite3_open");
    orig_sqlite3_open_v2 = dlsym(handle, "sqlite3_open_v2");
    orig_sqlite3_close = dlsym(handle, "sqlite3_close");
    orig_sqlite3_close_v2 = dlsym(handle, "sqlite3_close_v2");
    orig_sqlite3_exec = dlsym(handle, "sqlite3_exec");
    orig_sqlite3_changes = dlsym(handle, "sqlite3_changes");
    orig_sqlite3_changes64 = dlsym(handle, "sqlite3_changes64");
    orig_sqlite3_last_insert_rowid = dlsym(handle, "sqlite3_last_insert_rowid");
    orig_sqlite3_get_table = dlsym(handle, "sqlite3_get_table");

    orig_sqlite3_errmsg = dlsym(handle, "sqlite3_errmsg");
    orig_sqlite3_errcode = dlsym(handle, "sqlite3_errcode");
    orig_sqlite3_extended_errcode = dlsym(handle, "sqlite3_extended_errcode");

    orig_sqlite3_prepare = dlsym(handle, "sqlite3_prepare");
    orig_sqlite3_prepare_v2 = dlsym(handle, "sqlite3_prepare_v2");
    orig_sqlite3_prepare_v3 = dlsym(handle, "sqlite3_prepare_v3");
    orig_sqlite3_prepare16_v2 = dlsym(handle, "sqlite3_prepare16_v2");

    orig_sqlite3_bind_int = dlsym(handle, "sqlite3_bind_int");
    orig_sqlite3_bind_int64 = dlsym(handle, "sqlite3_bind_int64");
    orig_sqlite3_bind_double = dlsym(handle, "sqlite3_bind_double");
    orig_sqlite3_bind_text = dlsym(handle, "sqlite3_bind_text");
    orig_sqlite3_bind_text64 = dlsym(handle, "sqlite3_bind_text64");
    orig_sqlite3_bind_blob = dlsym(handle, "sqlite3_bind_blob");
    orig_sqlite3_bind_blob64 = dlsym(handle, "sqlite3_bind_blob64");
    orig_sqlite3_bind_value = dlsym(handle, "sqlite3_bind_value");
    orig_sqlite3_bind_null = dlsym(handle, "sqlite3_bind_null");

    orig_sqlite3_step = dlsym(handle, "sqlite3_step");
    orig_sqlite3_reset = dlsym(handle, "sqlite3_reset");
    orig_sqlite3_finalize = dlsym(handle, "sqlite3_finalize");
    orig_sqlite3_clear_bindings = dlsym(handle, "sqlite3_clear_bindings");

    orig_sqlite3_column_count = dlsym(handle, "sqlite3_column_count");
    orig_sqlite3_column_type = dlsym(handle, "sqlite3_column_type");
    orig_sqlite3_column_int = dlsym(handle, "sqlite3_column_int");
    orig_sqlite3_column_int64 = dlsym(handle, "sqlite3_column_int64");
    orig_sqlite3_column_double = dlsym(handle, "sqlite3_column_double");
    orig_sqlite3_column_text = dlsym(handle, "sqlite3_column_text");
    orig_sqlite3_column_blob = dlsym(handle, "sqlite3_column_blob");
    orig_sqlite3_column_bytes = dlsym(handle, "sqlite3_column_bytes");
    orig_sqlite3_column_name = dlsym(handle, "sqlite3_column_name");
    orig_sqlite3_column_value = dlsym(handle, "sqlite3_column_value");
    orig_sqlite3_data_count = dlsym(handle, "sqlite3_data_count");
    orig_sqlite3_db_handle = dlsym(handle, "sqlite3_db_handle");
    orig_sqlite3_expanded_sql = dlsym(handle, "sqlite3_expanded_sql");
    orig_sqlite3_sql = dlsym(handle, "sqlite3_sql");
    orig_sqlite3_free = dlsym(handle, "sqlite3_free");
    orig_sqlite3_bind_parameter_name = dlsym(handle, "sqlite3_bind_parameter_name");
    orig_sqlite3_bind_parameter_index = dlsym(handle, "sqlite3_bind_parameter_index");
    orig_sqlite3_column_decltype = dlsym(handle, "sqlite3_column_decltype");

    orig_sqlite3_value_type = dlsym(handle, "sqlite3_value_type");
    orig_sqlite3_value_text = dlsym(handle, "sqlite3_value_text");
    orig_sqlite3_value_int = dlsym(handle, "sqlite3_value_int");
    orig_sqlite3_value_int64 = dlsym(handle, "sqlite3_value_int64");
    orig_sqlite3_value_double = dlsym(handle, "sqlite3_value_double");
    orig_sqlite3_value_bytes = dlsym(handle, "sqlite3_value_bytes");
    orig_sqlite3_value_blob = dlsym(handle, "sqlite3_value_blob");

    orig_sqlite3_create_collation = dlsym(handle, "sqlite3_create_collation");
    orig_sqlite3_create_collation_v2 = dlsym(handle, "sqlite3_create_collation_v2");

    orig_sqlite3_malloc = dlsym(handle, "sqlite3_malloc");
    orig_sqlite3_bind_parameter_count = dlsym(handle, "sqlite3_bind_parameter_count");
    orig_sqlite3_stmt_readonly = dlsym(handle, "sqlite3_stmt_readonly");
    orig_sqlite3_stmt_busy = dlsym(handle, "sqlite3_stmt_busy");
    orig_sqlite3_stmt_status = dlsym(handle, "sqlite3_stmt_status");

    // Set up aliases
    real_sqlite3_prepare_v2 = orig_sqlite3_prepare_v2;
    real_sqlite3_errmsg = orig_sqlite3_errmsg;
    real_sqlite3_errcode = orig_sqlite3_errcode;

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

    // Detect fork and reset state if needed
    common_check_fork();

    // Install signal handlers (using common implementation)
    signal(SIGSEGV, common_signal_handler);
    signal(SIGBUS, common_signal_handler);
    signal(SIGFPE, common_signal_handler);
    signal(SIGILL, common_signal_handler);

    // Install fork handlers
    pthread_atfork(common_atfork_prepare, common_atfork_parent, common_atfork_child);
    fprintf(stderr, "[SHIM_INIT] Registered pthread_atfork handlers\n");
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

    // Initialize common modules
    common_shim_init_modules();

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

// Special case: decltype forwards to original (not my_*)
const char* sqlite3_column_decltype(sqlite3_stmt *s, int i) {
    return orig_sqlite3_column_decltype ? orig_sqlite3_column_decltype(s, i) : NULL;
}
