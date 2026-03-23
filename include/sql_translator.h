#ifndef SQL_TRANSLATOR_H
#define SQL_TRANSLATOR_H

/*
 * Compatibility wrapper around the generated translator ABI header.
 * The generated header owns the function declarations; this file preserves the
 * historical `sql_translation_t` alias used from C.
 */

#include "plex_pg_core_ffi.h"

typedef struct SqlTranslation sql_translation_t;

#endif /* SQL_TRANSLATOR_H */
