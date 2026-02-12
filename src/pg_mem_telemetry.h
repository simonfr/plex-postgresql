/*
 * PostgreSQL Shim - Memory Telemetry
 *
 * Lightweight runtime counters to attribute memory growth to shim subsystems.
 * Enabled only when PLEX_PG_MEM_TELEMETRY=1.
 */

#ifndef PG_MEM_TELEMETRY_H
#define PG_MEM_TELEMETRY_H

#include <stddef.h>

typedef enum {
    PMT_BIND_TEXT_ALLOC = 0,
    PMT_BIND_HEX_ALLOC,
    PMT_BIND_VALUE_BLOB_ALLOC,
    PMT_COLUMN_CACHED_BLOB_ALLOC,
    PMT_COLUMN_DECODED_BLOB_ALLOC,
    PMT_BIND_PARAM_REPLACE_FREE,
    PMT_STMT_SWEEP_EXTRA_FREE,
    PMT_COUNTER_MAX
} pg_mem_counter_t;

int pg_mem_telemetry_enabled(void);
void pg_mem_telemetry_add(pg_mem_counter_t counter, size_t bytes, unsigned long long events);
void pg_mem_telemetry_maybe_log(void);

#endif // PG_MEM_TELEMETRY_H
