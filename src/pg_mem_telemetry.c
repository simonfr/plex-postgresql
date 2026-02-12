/*
 * PostgreSQL Shim - Memory Telemetry
 *
 * Enabled by PLEX_PG_MEM_TELEMETRY=1.
 * Logs one summary line per 60 seconds to the shim log at ERROR level
 * (so it always shows up regardless of PLEX_PG_LOG_LEVEL).
 */

#include "pg_mem_telemetry.h"
#include "pg_logging.h"
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

/* Per-counter state: bytes accumulated and event count */
static atomic_ullong g_bytes[PMT_COUNTER_MAX];
static atomic_ullong g_events[PMT_COUNTER_MAX];

/* Previous snapshot for computing deltas */
static unsigned long long g_prev_bytes[PMT_COUNTER_MAX];
static unsigned long long g_prev_events[PMT_COUNTER_MAX];

static atomic_int g_enabled = -1;          /* -1 = not checked yet */
static atomic_ullong g_last_log_ts = 0;    /* seconds since epoch */

static const char *counter_names[PMT_COUNTER_MAX] = {
    "bind_text",
    "bind_hex",
    "bind_val_blob",
    "col_cached_blob",
    "col_decoded_blob",
    "bind_replace_free",
    "stmt_sweep_free"
};

int pg_mem_telemetry_enabled(void) {
    int v = atomic_load(&g_enabled);
    if (v == -1) {
        const char *env = getenv("PLEX_PG_MEM_TELEMETRY");
        v = (env && env[0] == '1') ? 1 : 0;
        atomic_store(&g_enabled, v);
    }
    return v;
}

void pg_mem_telemetry_add(pg_mem_counter_t counter, size_t bytes, unsigned long long events) {
    if (counter < 0 || counter >= PMT_COUNTER_MAX) return;
    atomic_fetch_add(&g_bytes[counter], (unsigned long long)bytes);
    atomic_fetch_add(&g_events[counter], events);
}

void pg_mem_telemetry_maybe_log(void) {
    if (!pg_mem_telemetry_enabled()) return;

    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    unsigned long long now = (unsigned long long)ts.tv_sec;
    unsigned long long prev = atomic_load(&g_last_log_ts);

    if (now - prev < 60) return;
    /* Attempt to claim this log slot (only one thread logs) */
    if (!atomic_compare_exchange_strong(&g_last_log_ts, &prev, now)) return;

    /* Collect snapshot and compute deltas */
    unsigned long long snap_bytes[PMT_COUNTER_MAX];
    unsigned long long snap_events[PMT_COUNTER_MAX];
    unsigned long long total_bytes = 0;
    unsigned long long total_events = 0;

    for (int i = 0; i < PMT_COUNTER_MAX; i++) {
        snap_bytes[i] = atomic_load(&g_bytes[i]);
        snap_events[i] = atomic_load(&g_events[i]);
    }

    /* Build compact log line */
    char buf[1024];
    int pos = 0;
    pos += snprintf(buf + pos, sizeof(buf) - pos, "MEM_TELEMETRY:");

    for (int i = 0; i < PMT_COUNTER_MAX; i++) {
        unsigned long long db = snap_bytes[i] - g_prev_bytes[i];
        unsigned long long de = snap_events[i] - g_prev_events[i];
        total_bytes += snap_bytes[i];
        total_events += snap_events[i];
        if (de > 0) {
            pos += snprintf(buf + pos, sizeof(buf) - pos,
                            " %s=%lluKB/%lluev(d:%lluKB/%lluev)",
                            counter_names[i],
                            snap_bytes[i] / 1024, snap_events[i],
                            db / 1024, de);
        }
        g_prev_bytes[i] = snap_bytes[i];
        g_prev_events[i] = snap_events[i];
    }

    pos += snprintf(buf + pos, sizeof(buf) - pos,
                    " TOTAL=%lluKB/%lluev", total_bytes / 1024, total_events);

    LOG_ERROR("%s", buf);
}
