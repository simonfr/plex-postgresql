/*
 * shim_alloc.c — Hybrid C allocator wrappers + Rust tracking backend
 *
 * This file provides the allocator wrapper functions that MUST remain in C
 * because:
 *   1. They call real libc malloc/free/realloc/calloc/strdup (without the
 *      macro overrides that shim_alloc.h installs for other translation units).
 *   2. They capture __FILE__ / __LINE__ at the call site via C macros in the
 *      header.
 *
 * All tracking state (atomic counters, hash table, stats, periodic logging)
 * lives in the Rust module rust/sql-translator/src/shim_alloc.rs and is
 * accessed here through the FFI functions declared below.
 *
 * Public API (defined in shim_alloc.h) is unchanged.
 */

/* Must be before shim_alloc.h to prevent macro redefinition of malloc/free */
#define SHIM_ALLOC_NO_OVERRIDE
#include "shim_alloc.h"

#include <stdlib.h>
#include <string.h>

/* ---- Rust FFI declarations ---- */

/* Returns the tracking mode: 0 = off, 1 = track, 2 = track+trace. */
extern int rust_shim_alloc_enabled(void);

/*
 * Record a new allocation in the Rust hash table.
 * Called AFTER a successful malloc/calloc/strdup so ptr is always non-NULL.
 */
extern void rust_shim_alloc_record(unsigned long long ptr,
                                    unsigned long long size,
                                    const char        *file,
                                    int                line);

/*
 * Remove a pointer from the Rust hash table.
 * Returns the previously recorded size (0 if not found).
 */
extern unsigned long long rust_shim_alloc_remove(unsigned long long ptr);

/* Increment allocation counters (total_allocs, bytes_alloc, bytes_live). */
extern void rust_shim_alloc_record_alloc(unsigned long long size);

/* Increment free counters (total_frees, bytes_freed, bytes_live). */
extern void rust_shim_alloc_record_free(unsigned long long size);

/* Increment realloc counters (total_reallocs, live bytes delta). */
extern void rust_shim_alloc_record_realloc(unsigned long long old_size,
                                            unsigned long long new_size);

/* Fill *out with a stats snapshot (identical layout to shim_alloc_stats_t). */
extern void rust_shim_alloc_get_stats(shim_alloc_stats_t *out);

/* Force-log a one-line summary now. */
extern void rust_shim_alloc_log_summary(void);

/* Log a summary if 60 s have elapsed since the last one. */
extern void rust_shim_alloc_maybe_log(void);

/* Log live allocation sites (stub in Rust backend — no file:line stored). */
extern void rust_shim_alloc_dump_leaks(void);

/* Reset all counters and clear the hash table. */
extern void rust_shim_alloc_reset(void);

/* ---- Allocator wrappers ---- */

/*
 * Each wrapper:
 *   1. Calls the real libc function (no macro override active here).
 *   2. If tracking is enabled AND the call succeeded, updates Rust state.
 *
 * The record + record_alloc calls are intentionally separate so the C side
 * can pass __FILE__/__LINE__ to rust_shim_alloc_record without the Rust side
 * needing to know about C preprocessor macros.
 */

void *shim_malloc_tracked(size_t size, const char *file, int line)
{
    void *ptr = malloc(size);
    if (ptr && rust_shim_alloc_enabled()) {
        rust_shim_alloc_record((unsigned long long)ptr,
                               (unsigned long long)size, file, line);
        rust_shim_alloc_record_alloc((unsigned long long)size);
    }
    return ptr;
}

void *shim_calloc_tracked(size_t count, size_t size, const char *file, int line)
{
    void  *ptr   = calloc(count, size);
    size_t total = count * size;
    if (ptr && rust_shim_alloc_enabled()) {
        rust_shim_alloc_record((unsigned long long)ptr,
                               (unsigned long long)total, file, line);
        rust_shim_alloc_record_alloc((unsigned long long)total);
    }
    return ptr;
}

void *shim_realloc_tracked(void *old_ptr, size_t new_size,
                            const char *file, int line)
{
    if (!rust_shim_alloc_enabled()) {
        return realloc(old_ptr, new_size);
    }

    /*
     * Remove the old pointer before calling realloc because after the call
     * old_ptr may be invalid (freed and reused by another thread).
     */
    unsigned long long old_size = rust_shim_alloc_remove((unsigned long long)old_ptr);
    void *ptr = realloc(old_ptr, new_size);
    if (ptr) {
        rust_shim_alloc_record((unsigned long long)ptr,
                               (unsigned long long)new_size, file, line);
        rust_shim_alloc_record_realloc(old_size, (unsigned long long)new_size);
    } else {
        /*
         * realloc failed; old_ptr is still valid. Re-insert it so the size
         * information is not permanently lost from the tracking table.
         */
        rust_shim_alloc_record((unsigned long long)old_ptr, old_size, file, line);
    }
    return ptr;
}

void shim_free_tracked(void *ptr, const char *file, int line)
{
    (void)file;
    (void)line;
    if (!ptr) return;
    if (rust_shim_alloc_enabled()) {
        unsigned long long size = rust_shim_alloc_remove((unsigned long long)ptr);
        rust_shim_alloc_record_free(size);
    }
    free(ptr);
}

char *shim_strdup_tracked(const char *s, const char *file, int line)
{
    if (!s) return NULL;
    size_t len = strlen(s) + 1;
    char *ptr = (char *)shim_malloc_tracked(len, file, line);
    if (ptr) memcpy(ptr, s, len);
    return ptr;
}

/* ---- Public API: delegate to Rust ---- */

void shim_alloc_get_stats(shim_alloc_stats_t *out)
{
    rust_shim_alloc_get_stats(out);
}

void shim_alloc_log_summary(void)
{
    rust_shim_alloc_log_summary();
}

void shim_alloc_maybe_log(void)
{
    rust_shim_alloc_maybe_log();
}

void shim_alloc_dump_leaks(void)
{
    rust_shim_alloc_dump_leaks();
}

void shim_alloc_reset(void)
{
    rust_shim_alloc_reset();
}
