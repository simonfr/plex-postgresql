/*
 * str_utils.h — Portable string utility functions
 *
 * Provides safe_strcasestr() as a replacement for musl's buggy strcasestr.
 * Used throughout the codebase (pg_config, db_interpose, etc.)
 */

#ifndef STR_UTILS_H
#define STR_UTILS_H

#include <stddef.h>
#include <strings.h>  // for strncasecmp

char* safe_strcasestr(const char *haystack, const char *needle);
char* str_replace_nocase(const char *str, const char *old, const char *new_str);

/* Replace system strcasestr with our safe version */
#ifdef strcasestr
#undef strcasestr
#endif
#define strcasestr safe_strcasestr

#endif /* STR_UTILS_H */
