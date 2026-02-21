/*
 * test_pool_exhaustion.c — Pool exhaustion simulation for Issue #9
 *
 * Simulates the exact scenario from Issue #9:
 *   - PLEX_PG_POOL_SIZE=50 (user's self-limiting setting)
 *   - Library scan (many concurrent connections) + 4 simultaneous streams
 *   - Measures: connection failures, latency spikes, timeout errors
 *
 * Unlike test_stress_load.c (which uses direct libpq, bypassing the pool),
 * this test simulates pool exhaustion by opening MORE concurrent connections
 * than the configured pool size, forcing contention.
 *
 * Since DYLD_INSERT_LIBRARIES doesn't work in standalone binaries, we simulate
 * the shim pool directly: N_POOL_SIZE connections are pre-allocated (like the
 * shim pool), and threads compete to borrow one. Threads that can't get a
 * connection within CHECKOUT_TIMEOUT_MS get a "pool exhaustion" error —
 * exactly what the shim returns when pool_get_connection() times out.
 *
 * Usage:
 *   ./tests/bin/test_pool_exhaustion [pool_size] [threads] [duration_sec]
 *
 * Example (reproducing Issue #9):
 *   ./tests/bin/test_pool_exhaustion 50 80 30
 *   (pool=50, 80 concurrent threads → exhaustion guaranteed)
 *
 * Example (with default pool):
 *   ./tests/bin/test_pool_exhaustion 150 80 30
 *   (pool=150, 80 threads → should be fine)
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

#define DEFAULT_POOL_SIZE    50
#define DEFAULT_THREADS      80    // > pool size to force exhaustion
#define DEFAULT_DURATION_SEC 30
#define MAX_THREADS          500
#define MAX_POOL_SIZE        500
#define LATENCY_BUCKETS      5000

// Checkout timeout: how long a thread waits for a pool connection (ms)
// The shim default is ~8s total (sum of retry delays). We use 5s here.
#define CHECKOUT_TIMEOUT_MS  5000

// ============================================================================
// Simulated connection pool
// ============================================================================

typedef struct {
    PGconn   *conn;
    _Atomic int in_use;   // 1 = borrowed, 0 = free
} pool_slot_t;

static pool_slot_t *g_pool      = NULL;
static int          g_pool_size = 0;

// Try to borrow a connection from the pool. Returns index or -1 (exhausted).
static int pool_acquire(int timeout_ms) {
    uint64_t deadline = 0;
    {
        struct timespec ts;
        clock_gettime(CLOCK_MONOTONIC, &ts);
        deadline = (uint64_t)ts.tv_sec * 1000ULL + ts.tv_nsec / 1000000ULL
                   + (uint64_t)timeout_ms;
    }

    while (1) {
        for (int i = 0; i < g_pool_size; i++) {
            int expected = 0;
            if (atomic_compare_exchange_weak(&g_pool[i].in_use, &expected, 1)) {
                return i;
            }
        }

        // Check timeout
        struct timespec ts;
        clock_gettime(CLOCK_MONOTONIC, &ts);
        uint64_t now_ms = (uint64_t)ts.tv_sec * 1000ULL + ts.tv_nsec / 1000000ULL;
        if (now_ms >= deadline) {
            return -1;  // POOL EXHAUSTED — timeout
        }

        // Back off briefly (shim uses exponential backoff; we use 10ms)
        usleep(10000);
    }
}

static void pool_release(int slot) {
    atomic_store(&g_pool[slot].in_use, 0);
}

// ============================================================================
// Shared state
// ============================================================================

static _Atomic int      g_running   = 1;
static _Atomic uint64_t g_total_ops = 0;
static _Atomic uint64_t g_pool_exhausted = 0;  // pool checkout timeouts
static _Atomic uint64_t g_query_errors   = 0;  // PG query errors

// Per-thread stats
typedef struct {
    uint64_t ops_ok;
    uint64_t ops_pool_timeout;  // couldn't get a connection
    uint64_t ops_query_err;     // got connection but query failed
    uint64_t latency_us[LATENCY_BUCKETS];
    int      latency_count;
    int      thread_id;
    int      is_scanner;  // 1=scanner (library scan), 0=stream
} thread_stats_t;

static thread_stats_t *g_stats;
static int             g_num_threads;
static int             g_duration_sec;
static int             g_configured_pool_size;

// Connection config
static const char *g_pg_host;
static const char *g_pg_database;
static const char *g_pg_user;
static const char *g_pg_password;
static const char *g_pg_schema;

// ============================================================================
// Timing
// ============================================================================

static uint64_t now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000ULL + (uint64_t)ts.tv_nsec / 1000ULL;
}

static void record_latency(thread_stats_t *st, uint64_t us) {
    if (st->latency_count < LATENCY_BUCKETS)
        st->latency_us[st->latency_count++] = us;
}

// ============================================================================
// Workloads
// ============================================================================

// Scanner: simulates Plex library scan (broad, many concurrent connections)
static const char *scanner_queries[] = {
    "SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 1",
    "SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 4",
    "SELECT COUNT(*) FROM media_items WHERE deleted_at IS NULL",
    "SELECT COUNT(*) FROM media_parts WHERE deleted_at IS NULL",
    "SELECT id, title, metadata_type, added_at FROM metadata_items WHERE metadata_type IN (1,4) ORDER BY id LIMIT 50",
    "SELECT id, directory_id, file FROM media_parts ORDER BY id LIMIT 50",
    "SELECT mi.id, mi.title, mp.file FROM metadata_items mi JOIN media_items mitem ON mitem.metadata_item_id = mi.id JOIN media_parts mp ON mp.media_item_id = mitem.id WHERE mi.metadata_type = 1 LIMIT 20",
    "SELECT COUNT(*) FROM taggings",
    "SELECT tag_id, metadata_item_id FROM taggings ORDER BY id LIMIT 100",
    "SELECT id, title, originally_available_at, rating FROM metadata_items ORDER BY updated_at DESC LIMIT 50",
};
static int scanner_query_count = (int)(sizeof(scanner_queries)/sizeof(scanner_queries[0]));

// Stream: simulates active Plex stream (repeated reads, quick)
static void run_stream_query(PGconn *conn, int thread_id, int *ok) {
    char sql[512];
    long base = 1000 + (long)(thread_id * 500);
    snprintf(sql, sizeof(sql),
        "SELECT id, title, originally_available_at, rating, summary "
        "FROM metadata_items WHERE id BETWEEN %ld AND %ld ORDER BY id LIMIT 10",
        base, base + 100);
    PGresult *res = PQexec(conn, sql);
    *ok = (PQresultStatus(res) == PGRES_TUPLES_OK);
    PQclear(res);
}

static void run_scanner_query(PGconn *conn, int *ok) {
    const char *sql = scanner_queries[rand() % scanner_query_count];
    PGresult *res = PQexec(conn, sql);
    *ok = (PQresultStatus(res) == PGRES_TUPLES_OK);
    PQclear(res);
}

// ============================================================================
// Thread function
// ============================================================================

typedef struct {
    int thread_id;
    int is_scanner;
} thread_arg_t;

static void *stress_thread(void *arg) {
    thread_arg_t  *ta  = (thread_arg_t *)arg;
    int            tid = ta->thread_id;
    thread_stats_t *st = &g_stats[tid];
    st->thread_id  = tid;
    st->is_scanner = ta->is_scanner;

    while (atomic_load(&g_running)) {
        uint64_t t0 = now_us();

        // Try to acquire a pool slot (simulates shim pool_get_connection)
        int slot = pool_acquire(CHECKOUT_TIMEOUT_MS);

        if (slot < 0) {
            // Pool exhausted — this is the Issue #9 failure mode
            st->ops_pool_timeout++;
            atomic_fetch_add(&g_pool_exhausted, 1);
            atomic_fetch_add(&g_total_ops, 1);
            uint64_t dt = now_us() - t0;
            record_latency(st, dt);
            // No sleep: immediately retry (simulates Plex retrying)
            continue;
        }

        PGconn *conn = g_pool[slot].conn;

        // Run query
        int query_ok = 0;
        if (ta->is_scanner) {
            run_scanner_query(conn, &query_ok);
        } else {
            run_stream_query(conn, tid, &query_ok);
        }

        pool_release(slot);

        uint64_t dt = now_us() - t0;

        if (query_ok) {
            st->ops_ok++;
        } else {
            st->ops_query_err++;
            atomic_fetch_add(&g_query_errors, 1);
        }
        record_latency(st, dt);
        atomic_fetch_add(&g_total_ops, 1);

        // Realistic inter-op sleep.
        // Streams: 500us-2ms between reads (actively streaming)
        // Scanners: NO sleep — Plex library scan runs queries back-to-back,
        //           holding the pool slot for the entire scan batch.
        if (!ta->is_scanner) {
            usleep(500 + (uint32_t)(rand() % 1500));
        }
    }

    return NULL;
}

// ============================================================================
// Stats helpers
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
// Progress
// ============================================================================

static void print_progress(int elapsed, int total) {
    uint64_t ops  = atomic_load(&g_total_ops);
    uint64_t exh  = atomic_load(&g_pool_exhausted);
    uint64_t qerr = atomic_load(&g_query_errors);
    printf("\r  [%3ds/%ds] ops=%-8llu  pool_timeout=%-6llu  query_err=%-4llu",
           elapsed, total,
           (unsigned long long)ops,
           (unsigned long long)exh,
           (unsigned long long)qerr);
    fflush(stdout);
}

// ============================================================================
// Pool init / teardown
// ============================================================================

static int pool_init(void) {
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

    printf("  Allocating pool of %d connections... ", g_pool_size);
    fflush(stdout);

    g_pool = calloc((size_t)g_pool_size, sizeof(pool_slot_t));
    if (!g_pool) { fprintf(stderr, "OOM\n"); return -1; }

    for (int i = 0; i < g_pool_size; i++) {
        g_pool[i].conn = PQconnectdb(connstr);
        if (PQstatus(g_pool[i].conn) != CONNECTION_OK) {
            fprintf(stderr, "\nFailed to open connection %d: %s\n",
                    i, PQerrorMessage(g_pool[i].conn));
            // Return what we have — test with reduced pool
            g_pool_size = i;
            printf("(got %d)\n", i);
            return 0;
        }
        // Set search_path
        char sp[256];
        snprintf(sp, sizeof(sp), "SET search_path TO %s", g_pg_schema);
        PGresult *r = PQexec(g_pool[i].conn, sp);
        PQclear(r);
        atomic_store(&g_pool[i].in_use, 0);

        if ((i+1) % 10 == 0) { printf("%d.. ", i+1); fflush(stdout); }
    }
    printf("OK\n");
    return 0;
}

static void pool_destroy(void) {
    for (int i = 0; i < g_pool_size; i++) {
        if (g_pool[i].conn) PQfinish(g_pool[i].conn);
    }
    free(g_pool);
}

// ============================================================================
// Main
// ============================================================================

int main(int argc, char **argv) {
    int pool_size    = DEFAULT_POOL_SIZE;
    int num_threads  = DEFAULT_THREADS;
    int duration_sec = DEFAULT_DURATION_SEC;

    if (argc >= 2) pool_size    = atoi(argv[1]);
    if (argc >= 3) num_threads  = atoi(argv[2]);
    if (argc >= 4) duration_sec = atoi(argv[3]);

    if (pool_size    < 1)            pool_size    = 1;
    if (pool_size    > MAX_POOL_SIZE) pool_size   = MAX_POOL_SIZE;
    if (num_threads  < 1)            num_threads  = 1;
    if (num_threads  > MAX_THREADS)  num_threads  = MAX_THREADS;
    if (duration_sec < 1)            duration_sec = 1;

    g_pool_size            = pool_size;
    g_num_threads          = num_threads;
    g_duration_sec         = duration_sec;
    g_configured_pool_size = pool_size;

    g_pg_host     = getenv("PLEX_PG_HOST");     if (!g_pg_host)     g_pg_host     = "/tmp";
    g_pg_database = getenv("PLEX_PG_DATABASE"); if (!g_pg_database) g_pg_database = "plex_stress";
    g_pg_user     = getenv("PLEX_PG_USER");     if (!g_pg_user)     g_pg_user     = "plex";
    g_pg_password = getenv("PLEX_PG_PASSWORD");
    g_pg_schema   = getenv("PLEX_PG_SCHEMA");   if (!g_pg_schema)   g_pg_schema   = "plex";

    printf("\n\033[1m=== Issue #9 Pool Exhaustion Simulation ===\033[0m\n");
    printf("  Pool size:  %d connections (PLEX_PG_POOL_SIZE=%d)\n", pool_size, pool_size);
    printf("  Threads:    %d (simulating library scan + %d streams)\n",
           num_threads, num_threads);
    printf("  Duration:   %ds\n", duration_sec);
    printf("  Database:   %s @ %s\n", g_pg_database, g_pg_host);
    printf("  Checkout timeout: %dms\n\n", CHECKOUT_TIMEOUT_MS);

    if (num_threads <= pool_size) {
        printf("  \033[33mWARNING: threads (%d) <= pool_size (%d) — pool exhaustion unlikely\033[0m\n\n",
               num_threads, pool_size);
    } else {
        printf("  \033[33mNOTE: %d threads competing for %d pool slots — expect exhaustion\033[0m\n\n",
               num_threads, pool_size);
    }

    // Init pool
    if (pool_init() < 0) return 1;
    if (g_pool_size == 0) {
        fprintf(stderr, "No connections available.\n");
        return 1;
    }

    // Allocate stats
    g_stats = calloc((size_t)num_threads, sizeof(thread_stats_t));
    if (!g_stats) { fprintf(stderr, "OOM\n"); return 1; }

    // Assign roles:
    //   First 4 threads = streams (simulating 4 active Plex streams)
    //   Rest = scanner threads (library scan)
    pthread_t    *threads = calloc((size_t)num_threads, sizeof(pthread_t));
    thread_arg_t *args    = calloc((size_t)num_threads, sizeof(thread_arg_t));

    int stream_threads  = 4;  // Issue #9: "3-4 streams"
    for (int i = 0; i < num_threads; i++) {
        args[i].thread_id  = i;
        args[i].is_scanner = (i >= stream_threads) ? 1 : 0;
    }

    printf("  Starting %d stream threads + %d scanner threads...\n\n",
           stream_threads, num_threads - stream_threads);

    for (int i = 0; i < num_threads; i++) {
        pthread_create(&threads[i], NULL, stress_thread, &args[i]);
    }

    // Run test
    uint64_t t_start = now_us();
    for (int elapsed = 0; elapsed < duration_sec; elapsed++) {
        sleep(1);
        // Accumulate for display
        uint64_t ok = 0, exh = 0, qerr = 0;
        for (int i = 0; i < num_threads; i++) {
            ok   += g_stats[i].ops_ok;
            exh  += g_stats[i].ops_pool_timeout;
            qerr += g_stats[i].ops_query_err;
        }
        atomic_store(&g_total_ops, ok + exh + qerr);
        atomic_store(&g_pool_exhausted, exh);
        atomic_store(&g_query_errors, qerr);
        print_progress(elapsed + 1, duration_sec);
    }

    atomic_store(&g_running, 0);
    for (int i = 0; i < num_threads; i++) pthread_join(threads[i], NULL);
    uint64_t t_end = now_us();
    double elapsed_sec = (double)(t_end - t_start) / 1e6;

    pool_destroy();

    printf("\n\n");

    // ========================================================================
    // Results
    // ========================================================================

    uint64_t total_ok = 0, total_exh = 0, total_qerr = 0;
    size_t   lat_cap = (size_t)num_threads * LATENCY_BUCKETS;
    uint64_t *all_lat = malloc(lat_cap * sizeof(uint64_t));
    int       all_lat_n = 0;

    for (int i = 0; i < num_threads; i++) {
        total_ok   += g_stats[i].ops_ok;
        total_exh  += g_stats[i].ops_pool_timeout;
        total_qerr += g_stats[i].ops_query_err;
        int n = g_stats[i].latency_count;
        if (all_lat_n + n <= (int)lat_cap) {
            memcpy(&all_lat[all_lat_n], g_stats[i].latency_us, (size_t)n * sizeof(uint64_t));
            all_lat_n += n;
        }
    }

    uint64_t total_ops = total_ok + total_exh + total_qerr;
    double err_pct    = total_ops > 0 ? 100.0 * (double)(total_exh + total_qerr) / (double)total_ops : 0.0;
    double exh_pct    = total_ops > 0 ? 100.0 * (double)total_exh  / (double)total_ops : 0.0;

    qsort(all_lat, (size_t)all_lat_n, sizeof(uint64_t), cmp_u64);

    printf("\033[1m=== Results ===\033[0m\n");
    printf("  Pool size configured: %d\n", g_configured_pool_size);
    printf("  Concurrent threads:   %d\n", num_threads);
    printf("  Duration:             %.1fs\n\n", elapsed_sec);
    printf("  Total ops:            %llu\n",  (unsigned long long)total_ops);
    printf("  Ops/sec:              %.1f\n",  (double)total_ops / elapsed_sec);
    printf("  Successes:            %llu\n",  (unsigned long long)total_ok);
    printf("  Pool timeouts:        %llu (%.2f%%) ← Issue #9 failure mode\n",
           (unsigned long long)total_exh, exh_pct);
    printf("  Query errors:         %llu\n",  (unsigned long long)total_qerr);
    printf("  Total error rate:     %.2f%%\n\n", err_pct);

    if (all_lat_n > 0) {
        printf("  Latency incl. pool wait (ms):\n");
        printf("    p50:  %.1f\n", (double)percentile(all_lat, all_lat_n, 50.0)  / 1000.0);
        printf("    p90:  %.1f\n", (double)percentile(all_lat, all_lat_n, 90.0)  / 1000.0);
        printf("    p95:  %.1f\n", (double)percentile(all_lat, all_lat_n, 95.0)  / 1000.0);
        printf("    p99:  %.1f\n", (double)percentile(all_lat, all_lat_n, 99.0)  / 1000.0);
        printf("    max:  %.1f\n", (double)all_lat[all_lat_n - 1] / 1000.0);
        printf("\n");
    }

    // Per-role breakdown
    uint64_t stream_ok = 0, stream_exh = 0, scanner_ok = 0, scanner_exh = 0;
    for (int i = 0; i < num_threads; i++) {
        if (!g_stats[i].is_scanner) {
            stream_ok  += g_stats[i].ops_ok;
            stream_exh += g_stats[i].ops_pool_timeout;
        } else {
            scanner_ok  += g_stats[i].ops_ok;
            scanner_exh += g_stats[i].ops_pool_timeout;
        }
    }
    printf("  By role:\n");
    printf("    STREAM   ok=%-6llu  pool_timeout=%llu\n",
           (unsigned long long)stream_ok,  (unsigned long long)stream_exh);
    printf("    SCANNER  ok=%-6llu  pool_timeout=%llu\n",
           (unsigned long long)scanner_ok, (unsigned long long)scanner_exh);
    printf("\n");

    // Verdict
    int passed = (err_pct < 1.0);
    if (passed) {
        printf("\033[32m  RESULT: PASS (error rate %.2f%% < 1%%)\033[0m\n", err_pct);
    } else {
        printf("\033[31m  RESULT: FAIL (error rate %.2f%% >= 1%%)\033[0m\n", err_pct);
        if (total_exh > 0) {
            printf("\033[31m  CAUSE:  Pool exhaustion — %llu operations timed out waiting\033[0m\n",
                   (unsigned long long)total_exh);
            printf("\033[31m          for a free connection slot (pool_size=%d, threads=%d)\033[0m\n",
                   g_configured_pool_size, num_threads);
        }
    }
    printf("\n");

    free(all_lat);
    free(g_stats);
    free(threads);
    free(args);
    return passed ? 0 : 1;
}
