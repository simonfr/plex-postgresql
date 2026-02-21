/*
 * test_stress_load.c — Concurrency stress test for plex-postgresql shim
 *
 * Simulates Issue #9: library scan + concurrent streams under load.
 * Runs N threads (default 20) for T seconds (default 30) using direct
 * libpq connections to PostgreSQL — same queries Plex would run, same
 * concurrency, but without SQLite interpose (which doesn't work in a
 * standalone binary on macOS).
 *
 * Usage:
 *   make test-stress                        (builds + runs with defaults)
 *   make test-stress STRESS_THREADS=40 STRESS_DURATION=60
 *   ./tests/bin/test_stress_load [threads] [duration_sec]
 *
 * Environment variables:
 *   PLEX_PG_HOST       Unix socket dir or hostname (default: /tmp)
 *   PLEX_PG_DATABASE   Database name (default: plex)
 *   PLEX_PG_USER       Username (default: plex)
 *   PLEX_PG_PASSWORD   Password (default: none)
 *   PLEX_PG_SCHEMA     Schema name (default: plex)
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stdatomic.h>
#include <pthread.h>
#include <unistd.h>
#include <time.h>
#include <math.h>
#include <libpq-fe.h>

// ============================================================================
// Configuration
// ============================================================================

#define DEFAULT_THREADS       20
#define DEFAULT_DURATION_SEC  30
#define MAX_THREADS           200
#define LATENCY_BUCKETS       10000   // stores up to 10k latency samples per thread

// Thread roles (simulating different Plex workloads)
typedef enum {
    ROLE_SCANNER = 0,  // library scan: many SELECTs on metadata_items, media_items
    ROLE_STREAM  = 1,  // streaming: repeated reads on specific metadata
    ROLE_WRITER  = 2,  // metadata refresh: UPDATE/INSERT on taggings, media_item_settings
    ROLE_MIXED   = 3,  // mixed read+write (realistic Plex thread)
} thread_role_t;

// ============================================================================
// Shared state
// ============================================================================

static _Atomic int      g_running   = 1;
static _Atomic uint64_t g_total_ok  = 0;
static _Atomic uint64_t g_total_err = 0;
static _Atomic uint64_t g_total_ops = 0;

// Per-thread stats
typedef struct {
    uint64_t      ops_ok;
    uint64_t      ops_err;
    uint64_t      latency_us[LATENCY_BUCKETS];
    int           latency_count;
    thread_role_t role;
    int           thread_id;
} thread_stats_t;

static thread_stats_t *g_stats;
static int             g_num_threads;
static int             g_duration_sec;

// Connection config (from env)
static const char *g_pg_host;
static const char *g_pg_database;
static const char *g_pg_user;
static const char *g_pg_password;
static const char *g_pg_schema;

// ============================================================================
// Timing helpers
// ============================================================================

static uint64_t now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000ULL + (uint64_t)ts.tv_nsec / 1000ULL;
}

static void record_latency(thread_stats_t *st, uint64_t us) {
    if (st->latency_count < LATENCY_BUCKETS) {
        st->latency_us[st->latency_count++] = us;
    }
}

// ============================================================================
// PG connection helpers
// ============================================================================

static PGconn *pg_connect(void) {
    // Build connection string
    char connstr[1024];
    if (g_pg_password && g_pg_password[0]) {
        snprintf(connstr, sizeof(connstr),
            "host=%s dbname=%s user=%s password=%s",
            g_pg_host, g_pg_database, g_pg_user, g_pg_password);
    } else {
        snprintf(connstr, sizeof(connstr),
            "host=%s dbname=%s user=%s",
            g_pg_host, g_pg_database, g_pg_user);
    }

    PGconn *conn = PQconnectdb(connstr);
    if (PQstatus(conn) != CONNECTION_OK) {
        fprintf(stderr, "Connection failed: %s\n", PQerrorMessage(conn));
        PQfinish(conn);
        return NULL;
    }

    // Set search_path to match what Plex shim does
    char set_schema[256];
    snprintf(set_schema, sizeof(set_schema), "SET search_path TO %s", g_pg_schema);
    PGresult *res = PQexec(conn, set_schema);
    PQclear(res);

    return conn;
}

// Run a SELECT query, return number of rows or -1 on error
static int pg_run_select(PGconn *conn, const char *sql) {
    PGresult *res = PQexec(conn, sql);
    ExecStatusType status = PQresultStatus(res);
    int rows = -1;
    if (status == PGRES_TUPLES_OK) {
        rows = PQntuples(res);
    }
    PQclear(res);

    // If connection is broken, try to reset it
    if (status == PGRES_FATAL_ERROR && PQstatus(conn) != CONNECTION_OK) {
        PQreset(conn);
    }
    return rows;
}

// ============================================================================
// Workload definitions
// ============================================================================

// SCANNER: simulates Plex library scan — broad reads across metadata
static void workload_scanner(PGconn *conn, thread_stats_t *st) {
    static const char *queries[] = {
        // Count queries (cheap, common during scan)
        "SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 1",
        "SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 4",
        "SELECT COUNT(*) FROM media_items WHERE deleted_at IS NULL",
        "SELECT COUNT(*) FROM media_parts WHERE deleted_at IS NULL",
        // Range scan (simulates Plex fetching library chunks)
        "SELECT id, title, metadata_type, added_at FROM metadata_items "
        "  WHERE metadata_type IN (1,4) ORDER BY id LIMIT 50",
        "SELECT id, directory_id, file FROM media_parts "
        "  ORDER BY id LIMIT 50",
        // Join (realistic Plex scan query)
        "SELECT mi.id, mi.title, mp.file FROM metadata_items mi "
        "  JOIN media_items mitem ON mitem.metadata_item_id = mi.id "
        "  JOIN media_parts mp ON mp.media_item_id = mitem.id "
        "  WHERE mi.metadata_type = 1 LIMIT 20",
        // Taggings (very large table)
        "SELECT COUNT(*) FROM taggings",
        "SELECT tag_id, metadata_item_id FROM taggings ORDER BY id LIMIT 100",
    };
    int n = (int)(sizeof(queries) / sizeof(queries[0]));
    const char *sql = queries[rand() % n];

    uint64_t t0 = now_us();
    int rc = pg_run_select(conn, sql);
    uint64_t dt = now_us() - t0;

    if (rc >= 0) { st->ops_ok++; } else { st->ops_err++; }
    record_latency(st, dt);
}

// STREAM: simulates a Plex stream — repeated reads on the same metadata item
static void workload_stream(PGconn *conn, thread_stats_t *st, int thread_id) {
    long base_id = 1000 + (thread_id * 500);
    char sql[512];

    if (rand() % 2 == 0) {
        snprintf(sql, sizeof(sql),
            "SELECT id, title, originally_available_at, rating, summary "
            "FROM metadata_items WHERE id BETWEEN %ld AND %ld "
            "ORDER BY id LIMIT 10", base_id, base_id + 100);
    } else {
        snprintf(sql, sizeof(sql),
            "SELECT ms.id, ms.media_item_id, ms.settings "
            "FROM media_item_settings ms "
            "WHERE ms.account_id = 1 LIMIT 20");
    }

    uint64_t t0 = now_us();
    int rc = pg_run_select(conn, sql);
    uint64_t dt = now_us() - t0;

    if (rc >= 0) { st->ops_ok++; } else { st->ops_err++; }
    record_latency(st, dt);
}

// WRITER: simulates metadata refresh writes — safe reads on live data
static void workload_writer(PGconn *conn, thread_stats_t *st) {
    // Read a row and "simulate" an update via a read (safe: no actual writes to Plex data)
    static const char *queries[] = {
        "SELECT id FROM media_item_settings WHERE account_id = 1 LIMIT 1",
        "SELECT id FROM metadata_items WHERE metadata_type = 1 ORDER BY updated_at DESC LIMIT 5",
        "SELECT id, tag_id FROM taggings ORDER BY id DESC LIMIT 10",
    };
    int n = (int)(sizeof(queries) / sizeof(queries[0]));
    const char *sql = queries[rand() % n];

    uint64_t t0 = now_us();
    int rc = pg_run_select(conn, sql);
    uint64_t dt = now_us() - t0;

    if (rc >= 0) { st->ops_ok++; } else { st->ops_err++; }
    record_latency(st, dt);
}

// MIXED: alternates between read and write patterns
static void workload_mixed(PGconn *conn, thread_stats_t *st, int thread_id) {
    int r = rand() % 4;
    if (r < 3) {
        workload_scanner(conn, st);
    } else {
        workload_writer(conn, st);
    }
    (void)thread_id;
}

// ============================================================================
// Thread function
// ============================================================================

typedef struct {
    int           thread_id;
    thread_role_t role;
} thread_arg_t;

static void *stress_thread(void *arg) {
    thread_arg_t  *ta   = (thread_arg_t *)arg;
    int            tid  = ta->thread_id;
    thread_role_t  role = ta->role;
    thread_stats_t *st  = &g_stats[tid];
    st->thread_id = tid;
    st->role      = role;

    PGconn *conn = pg_connect();
    if (!conn) {
        st->ops_err++;
        return NULL;
    }

    while (atomic_load(&g_running)) {
        switch (role) {
            case ROLE_SCANNER: workload_scanner(conn, st);           break;
            case ROLE_STREAM:  workload_stream(conn, st, tid);       break;
            case ROLE_WRITER:  workload_writer(conn, st);            break;
            case ROLE_MIXED:   workload_mixed(conn, st, tid);        break;
        }
        atomic_fetch_add(&g_total_ops, 1);

        // Realistic Plex inter-operation sleep: 1–6ms
        usleep(1000 + (uint32_t)(rand() % 5000));
    }

    PQfinish(conn);
    return NULL;
}

// ============================================================================
// Stats: percentile calculation
// ============================================================================

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}

static uint64_t percentile(uint64_t *arr, int n, double p) {
    if (n == 0) return 0;
    int idx = (int)(p / 100.0 * (double)n);
    if (idx >= n) idx = n - 1;
    return arr[idx];
}

// ============================================================================
// Progress reporter (runs on main thread during test)
// ============================================================================

static void print_progress(int elapsed, int total) {
    uint64_t ops = atomic_load(&g_total_ops);
    uint64_t ok  = atomic_load(&g_total_ok);
    uint64_t err = atomic_load(&g_total_err);
    printf("\r  [%3ds/%ds] ops=%-8llu  ok=%-8llu  err=%-6llu",
           elapsed, total,
           (unsigned long long)ops,
           (unsigned long long)ok,
           (unsigned long long)err);
    fflush(stdout);
}

// ============================================================================
// Main
// ============================================================================

int main(int argc, char **argv) {
    int num_threads  = DEFAULT_THREADS;
    int duration_sec = DEFAULT_DURATION_SEC;

    if (argc >= 2) num_threads  = atoi(argv[1]);
    if (argc >= 3) duration_sec = atoi(argv[2]);

    if (num_threads  < 1)           num_threads  = 1;
    if (num_threads  > MAX_THREADS) num_threads  = MAX_THREADS;
    if (duration_sec < 1)           duration_sec = 1;

    // Connection config from env
    g_pg_host     = getenv("PLEX_PG_HOST");     if (!g_pg_host)     g_pg_host     = "/tmp";
    g_pg_database = getenv("PLEX_PG_DATABASE"); if (!g_pg_database) g_pg_database = "plex";
    g_pg_user     = getenv("PLEX_PG_USER");     if (!g_pg_user)     g_pg_user     = "plex";
    g_pg_password = getenv("PLEX_PG_PASSWORD"); // NULL is fine
    g_pg_schema   = getenv("PLEX_PG_SCHEMA");   if (!g_pg_schema)   g_pg_schema   = "plex";

    g_num_threads  = num_threads;
    g_duration_sec = duration_sec;

    printf("\n\033[1m=== Shim Stress Test (direct libpq) ===\033[0m\n");
    printf("  Threads:  %d\n", num_threads);
    printf("  Duration: %ds\n", duration_sec);
    printf("  Host:     %s\n", g_pg_host);
    printf("  Database: %s\n", g_pg_database);
    printf("  Schema:   %s\n\n", g_pg_schema);

    // Verify connectivity before starting threads
    printf("  Verifying database connection... ");
    fflush(stdout);
    PGconn *test_conn = pg_connect();
    if (!test_conn) {
        printf("FAILED\n\n");
        fprintf(stderr, "Cannot connect to PostgreSQL. Check PLEX_PG_HOST, PLEX_PG_DATABASE, PLEX_PG_USER.\n");
        return 1;
    }
    // Quick sanity: count metadata_items
    PGresult *res = PQexec(test_conn, "SELECT COUNT(*) FROM metadata_items");
    if (PQresultStatus(res) == PGRES_TUPLES_OK) {
        printf("OK (%s metadata_items)\n\n", PQgetvalue(res, 0, 0));
    } else {
        printf("OK (table check skipped: %s)\n\n", PQerrorMessage(test_conn));
    }
    PQclear(res);
    PQfinish(test_conn);

    // Allocate per-thread stats
    g_stats = calloc((size_t)num_threads, sizeof(thread_stats_t));
    if (!g_stats) { fprintf(stderr, "OOM\n"); return 1; }

    // Assign roles: 25% scanners, 40% streams, 10% writers, 25% mixed
    pthread_t    *threads = calloc((size_t)num_threads, sizeof(pthread_t));
    thread_arg_t *args    = calloc((size_t)num_threads, sizeof(thread_arg_t));

    for (int i = 0; i < num_threads; i++) {
        int r = (i * 100) / num_threads;
        if      (r < 25)  args[i].role = ROLE_SCANNER;
        else if (r < 65)  args[i].role = ROLE_STREAM;
        else if (r < 75)  args[i].role = ROLE_WRITER;
        else              args[i].role = ROLE_MIXED;
        args[i].thread_id = i;
    }

    // Start threads
    for (int i = 0; i < num_threads; i++) {
        pthread_create(&threads[i], NULL, stress_thread, &args[i]);
    }

    // Run for duration, updating progress
    uint64_t t_start = now_us();
    for (int elapsed = 0; elapsed < duration_sec; elapsed++) {
        sleep(1);
        // Accumulate per-thread ok/err into globals for display
        uint64_t total_ok = 0, total_err = 0;
        for (int i = 0; i < num_threads; i++) {
            total_ok  += g_stats[i].ops_ok;
            total_err += g_stats[i].ops_err;
        }
        atomic_store(&g_total_ok,  total_ok);
        atomic_store(&g_total_err, total_err);
        print_progress(elapsed + 1, duration_sec);
    }

    // Signal threads to stop
    atomic_store(&g_running, 0);
    for (int i = 0; i < num_threads; i++) {
        pthread_join(threads[i], NULL);
    }
    uint64_t t_end = now_us();
    double elapsed_sec = (double)(t_end - t_start) / 1e6;

    printf("\n\n");

    // ========================================================================
    // Aggregate stats
    // ========================================================================

    uint64_t total_ok = 0, total_err = 0, total_ops = 0;

    // Collect all latencies — allocate on heap to avoid stack overflow with MAX_THREADS
    size_t lat_cap = (size_t)num_threads * LATENCY_BUCKETS;
    uint64_t *all_latencies = malloc(lat_cap * sizeof(uint64_t));
    int all_lat_count = 0;

    for (int i = 0; i < num_threads; i++) {
        total_ok  += g_stats[i].ops_ok;
        total_err += g_stats[i].ops_err;
        total_ops += g_stats[i].ops_ok + g_stats[i].ops_err;
        int n = g_stats[i].latency_count;
        if (all_lat_count + n <= (int)lat_cap) {
            memcpy(&all_latencies[all_lat_count], g_stats[i].latency_us, (size_t)n * sizeof(uint64_t));
            all_lat_count += n;
        }
    }

    // Sort for percentiles
    qsort(all_latencies, (size_t)all_lat_count, sizeof(uint64_t), cmp_u64);

    double ops_per_sec = (double)total_ops / elapsed_sec;
    double err_pct     = total_ops > 0 ? (100.0 * (double)total_err / (double)total_ops) : 0.0;

    printf("\033[1m=== Results ===\033[0m\n");
    printf("  Duration:      %.1fs\n", elapsed_sec);
    printf("  Total ops:     %llu\n",  (unsigned long long)total_ops);
    printf("  Ops/sec:       %.1f\n",  ops_per_sec);
    printf("  Successes:     %llu\n",  (unsigned long long)total_ok);
    printf("  Errors:        %llu (%.2f%%)\n", (unsigned long long)total_err, err_pct);
    printf("\n");

    if (all_lat_count > 0) {
        printf("  Latency (ms):\n");
        printf("    p50:  %.1f\n", (double)percentile(all_latencies, all_lat_count, 50.0)  / 1000.0);
        printf("    p90:  %.1f\n", (double)percentile(all_latencies, all_lat_count, 90.0)  / 1000.0);
        printf("    p95:  %.1f\n", (double)percentile(all_latencies, all_lat_count, 95.0)  / 1000.0);
        printf("    p99:  %.1f\n", (double)percentile(all_latencies, all_lat_count, 99.0)  / 1000.0);
        printf("    max:  %.1f\n", (double)all_latencies[all_lat_count - 1] / 1000.0);
        printf("\n");
    }

    // Per-role breakdown
    printf("  By role:\n");
    const char *role_names[] = {"SCANNER", "STREAM ", "WRITER ", "MIXED  "};
    for (int role = 0; role < 4; role++) {
        uint64_t rok = 0, rerr = 0;
        for (int i = 0; i < num_threads; i++) {
            if ((int)g_stats[i].role == role) {
                rok  += g_stats[i].ops_ok;
                rerr += g_stats[i].ops_err;
            }
        }
        if (rok + rerr > 0) {
            printf("    %s  ok=%-6llu  err=%llu\n",
                   role_names[role],
                   (unsigned long long)rok,
                   (unsigned long long)rerr);
        }
    }

    printf("\n");

    // Pass/fail verdict
    int passed = (err_pct < 1.0);
    if (passed) {
        printf("\033[32m  RESULT: PASS (error rate %.2f%% < 1%%)\033[0m\n\n", err_pct);
    } else {
        printf("\033[31m  RESULT: FAIL (error rate %.2f%% >= 1%%)\033[0m\n\n", err_pct);
    }

    free(all_latencies);
    free(g_stats);
    free(threads);
    free(args);
    return passed ? 0 : 1;
}
