/*
 * str_utils.c — Portable string utility functions
 *
 * Extracted from sql_tr_helpers.c during the C→Rust translator migration.
 * Provides safe_strcasestr() used by pg_config, db_interpose_prepare, etc.
 */

#include "../include/str_utils.h"
#include <string.h>
#include <stdlib.h>

char* safe_strcasestr(const char *haystack, const char *needle) {
    if (!haystack || !needle) return NULL;
    if (!*needle) return (char*)haystack;

    size_t needle_len = strlen(needle);

    for (const char *p = haystack; *p; p++) {
        if (strncasecmp(p, needle, needle_len) == 0) {
            return (char*)p;
        }
    }
    return NULL;
}

/* Case-insensitive string replace (single-pass) */
char* str_replace_nocase(const char *str, const char *old, const char *new_str) {
    if (!str || !old || !new_str) return NULL;

    size_t old_len = strlen(old);
    if (old_len == 0) return strdup(str);

    size_t new_len = strlen(new_str);
    size_t str_len = strlen(str);
    size_t buf_size = str_len + 256;
    char *result = malloc(buf_size);
    if (!result) return NULL;

    char *out = result;
    const char *p = str;
    const char *match;

    while ((match = safe_strcasestr(p, old)) != NULL) {
        size_t prefix_len = match - p;
        size_t used = out - result;
        size_t needed = used + prefix_len + new_len + strlen(match + old_len) + 1;
        if (needed > buf_size) {
            buf_size = needed + 64;
            char *new_buf = realloc(result, buf_size);
            if (!new_buf) { free(result); return NULL; }
            out = new_buf + used;
            result = new_buf;
        }
        memcpy(out, p, prefix_len);
        out += prefix_len;
        memcpy(out, new_str, new_len);
        out += new_len;
        p = match + old_len;
    }

    size_t remainder = strlen(p);
    size_t used = out - result;
    if (used + remainder + 1 > buf_size) {
        char *new_buf = realloc(result, used + remainder + 1);
        if (!new_buf) { free(result); return NULL; }
        out = new_buf + used;
        result = new_buf;
    }
    memcpy(out, p, remainder);
    out += remainder;
    *out = '\0';

    return result;
}
