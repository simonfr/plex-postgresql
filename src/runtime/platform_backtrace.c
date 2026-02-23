/*
 * Plex PostgreSQL Interposing Shim - Backtrace Module
 *
 * Unified backtrace implementation for macOS and Linux.
 * - macOS: uses execinfo.h (backtrace/backtrace_symbols)
 * - Linux: uses manual frame walking + /proc/self/maps + dladdr
 * - Shared: box-drawing rendering, symbol demangling, LOG_ERROR output
 */

#ifdef __APPLE__
#include <execinfo.h>
#else
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <dlfcn.h>
#endif

#include "db_interpose.h"
#include "db_interpose_common.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// ============================================================================
// Constants
// ============================================================================

#define MAX_FRAMES       64
#define MAX_DISPLAY      25
#define MAX_FUNC_LEN     70

// ============================================================================
// Frame Collection (platform-specific)
// ============================================================================

#ifdef __APPLE__

static int collect_frames(void **frames, int max_frames) {
    return backtrace(frames, max_frames);
}

#else /* Linux */

static int collect_frames(void **frames, int max_frames) {
    int depth = 0;
    void **fp = __builtin_frame_address(0);
    int iterations = 0;

    while (fp && depth < max_frames && iterations < 100) {
        iterations++;

        if ((unsigned long)fp < 0x1000 || (unsigned long)fp > 0xffffffffffff) break;

        void *ret_addr = NULL;
        #if defined(__aarch64__) || defined(__x86_64__)
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

#endif /* __APPLE__ */

// ============================================================================
// Symbol Resolution (platform-specific)
// ============================================================================

typedef struct {
    char func[256];
    char lib[256];
} resolved_symbol_t;

#ifdef __APPLE__

static void resolve_symbols(void **frames, int count, resolved_symbol_t *out) {
    char **symbols = backtrace_symbols(frames, count);

    for (int i = 0; i < count; i++) {
        out[i].func[0] = '\0';
        out[i].lib[0] = '\0';

        if (!symbols || !symbols[i]) continue;

        char *symbol = symbols[i];

        // Parse backtrace_symbols format: "idx  libname  addr funcname + offset"
        // Find the mangled symbol name before the '+' sign
        char *plus_sign = strrchr(symbol, '+');
        char *name_start = NULL;
        if (plus_sign) {
            char *p = plus_sign - 1;
            while (p > symbol && *p == ' ') p--;
            while (p > symbol && *p != ' ') p--;
            if (*p == ' ') name_start = p + 1;
        }

        if (name_start && cxa_demangle_fn) {
            size_t name_len = plus_sign - name_start - 1;
            char mangled[256];
            if (name_len < sizeof(mangled)) {
                strncpy(mangled, name_start, name_len);
                mangled[name_len] = '\0';

                int status = 0;
                char *demangled = cxa_demangle_fn(mangled, NULL, NULL, &status);
                if (demangled && status == 0) {
                    strncpy(out[i].func, demangled, sizeof(out[i].func) - 1);
                    free(demangled);
                } else {
                    strncpy(out[i].func, mangled, sizeof(out[i].func) - 1);
                }
            }
        }

        // If no demangled name, use the raw symbol line
        if (!out[i].func[0]) {
            strncpy(out[i].func, symbol, sizeof(out[i].func) - 1);
        }
    }

    free(symbols);
}

#else /* Linux */

// /proc/self/maps parsing for library name resolution

#define MAX_MAPS_ENTRIES 256

typedef struct {
    unsigned long start;
    unsigned long end;
    char path[256];
} map_entry_t;

static map_entry_t memory_map[MAX_MAPS_ENTRIES];
static int memory_map_count = 0;

static void load_memory_map(void) {
    memory_map_count = 0;

    FILE *maps = fopen("/proc/self/maps", "r");
    if (!maps) return;

    char line[512];
    while (fgets(line, sizeof(line), maps) && memory_map_count < MAX_MAPS_ENTRIES) {
        map_entry_t *e = &memory_map[memory_map_count];

        // Parse "start-end perms offset dev inode pathname"
        unsigned long start = 0, end = 0;
        char *p = line;

        // Parse start address
        while (*p && *p != '-') {
            char c = *p;
            if (c >= '0' && c <= '9') start = (start << 4) | (c - '0');
            else if (c >= 'a' && c <= 'f') start = (start << 4) | (c - 'a' + 10);
            else if (c >= 'A' && c <= 'F') start = (start << 4) | (c - 'A' + 10);
            p++;
        }
        if (*p != '-') continue;
        p++;

        // Parse end address
        while (*p && *p != ' ') {
            char c = *p;
            if (c >= '0' && c <= '9') end = (end << 4) | (c - '0');
            else if (c >= 'a' && c <= 'f') end = (end << 4) | (c - 'a' + 10);
            else if (c >= 'A' && c <= 'F') end = (end << 4) | (c - 'A' + 10);
            p++;
        }

        // Skip perms, offset, dev, inode (4 space-delimited fields)
        for (int skip = 0; skip < 4 && *p; skip++) {
            while (*p == ' ') p++;
            while (*p && *p != ' ' && *p != '\n') p++;
        }
        while (*p == ' ') p++;

        // Remaining is pathname
        e->start = start;
        e->end = end;
        if (*p && *p != '\n') {
            int plen = 0;
            while (*p && *p != '\n' && plen < 255) {
                e->path[plen++] = *p++;
            }
            e->path[plen] = '\0';
        } else {
            strcpy(e->path, "[anonymous]");
        }

        memory_map_count++;
    }

    fclose(maps);
}

static const char* find_lib_for_addr(unsigned long addr) {
    for (int i = 0; i < memory_map_count; i++) {
        if (addr >= memory_map[i].start && addr < memory_map[i].end) {
            const char *base = strrchr(memory_map[i].path, '/');
            return base ? base + 1 : memory_map[i].path;
        }
    }
    return "[unknown]";
}

static void resolve_symbols(void **frames, int count, resolved_symbol_t *out) {
    load_memory_map();

    for (int i = 0; i < count; i++) {
        out[i].func[0] = '\0';
        out[i].lib[0] = '\0';

        unsigned long addr = (unsigned long)frames[i];

        Dl_info info;
        if (dladdr(frames[i], &info)) {
            if (info.dli_fname) {
                const char *base = strrchr(info.dli_fname, '/');
                strncpy(out[i].lib, base ? base + 1 : info.dli_fname, sizeof(out[i].lib) - 1);
            }

            if (info.dli_sname) {
                if (cxa_demangle_fn) {
                    int status = 0;
                    char *demangled = cxa_demangle_fn(info.dli_sname, NULL, NULL, &status);
                    if (demangled) {
                        strncpy(out[i].func, demangled, sizeof(out[i].func) - 1);
                        free(demangled);
                    } else {
                        strncpy(out[i].func, info.dli_sname, sizeof(out[i].func) - 1);
                    }
                } else {
                    strncpy(out[i].func, info.dli_sname, sizeof(out[i].func) - 1);
                }
            }
        }

        if (!out[i].lib[0]) {
            strncpy(out[i].lib, find_lib_for_addr(addr), sizeof(out[i].lib) - 1);
        }

        if (!out[i].func[0]) {
            snprintf(out[i].func, sizeof(out[i].func), "[%p]", frames[i]);
        }
    }
}

#endif /* __APPLE__ */

// ============================================================================
// Shared Backtrace Rendering
// ============================================================================

void platform_print_backtrace(const char *reason, int skip_frames) {
    void *frames[MAX_FRAMES];
    int depth = collect_frames(frames, MAX_FRAMES);

    if (depth == 0) {
        fprintf(stderr, "\n  [Stack trace unavailable]\n");
        return;
    }

    fprintf(stderr, "\n");
    fprintf(stderr, "╔══════════════════════════════════════════════════════════════════════════════╗\n");
    fprintf(stderr, "║ BACKTRACE: %-67s ║\n", reason ? reason : "Unknown");
    fprintf(stderr, "╠══════════════════════════════════════════════════════════════════════════════╣\n");
    LOG_ERROR("=== BACKTRACE (%s) ===", reason ? reason : "Unknown");

    resolved_symbol_t *symbols = calloc(depth, sizeof(resolved_symbol_t));
    if (!symbols) return;

    resolve_symbols(frames, depth, symbols);

    int printed = 0;
    for (int i = skip_frames; i < depth && printed < MAX_DISPLAY; i++) {
        // Truncate long function names
        if (strlen(symbols[i].func) > MAX_FUNC_LEN) {
            symbols[i].func[MAX_FUNC_LEN - 3] = '.';
            symbols[i].func[MAX_FUNC_LEN - 2] = '.';
            symbols[i].func[MAX_FUNC_LEN - 1] = '.';
            symbols[i].func[MAX_FUNC_LEN] = '\0';
        }

        char line[256];
        snprintf(line, sizeof(line), "[%2d] %s", printed, symbols[i].func);

        fprintf(stderr, "║ %-78s ║\n", line);
        LOG_ERROR("  %s", line);
        printed++;
    }

    if (depth > skip_frames + MAX_DISPLAY) {
        fprintf(stderr, "║ ... and %d more frames                                                         ║\n",
                depth - skip_frames - MAX_DISPLAY);
    }
    fprintf(stderr, "╚══════════════════════════════════════════════════════════════════════════════╝\n");
    fflush(stderr);

    free(symbols);
}
