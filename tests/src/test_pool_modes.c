/*
 * test_pool_modes.c — A/B/C comparison of pool strategies for Issue #9
 *
 * Simulates three pool strategies under identical load (80 threads, 50 pool slots):
 *
 *   thread  — Current shim behavior: each thread holds a slot for its entire lifetime.
 *             Threads that can't get a slot at startup are permanently locked out.
 *
 *   borrow  — Classic borrow-return: acquire before each query, release after.
 *             No thread affinity, pure first-come-first-served.
 *
 *   idle    — Proposed SLOT_IDLE: after a query, slot goes to IDLE state with
 *             last_owner tracking. Same thread can reclaim instantly (TLS fast path).
 *             Under pressure, other threads can steal IDLE slots.
 *
 * Usage:
 *   ./tests/bin/test_pool_modes <mode> [pool_size] [threads] [duration_sec]
 *
 * Examples:
 *   ./tests/bin/test_pool_modes thread  50 80 30
 *   ./tests/bin/test_pool_modes borrow  50 80 30
 *   ./tests/bin/test_pool_modes idle    50 80 30
 *
 * Environment:
 *   PLEX_PG_HOST, PLEX_PG_DATABASE, PLEX_PG_USER, PLEX_PG_PASSWORD, PLEX_PG_SCHEMA
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
#define DEFAULT_THREADS      80
#define DEFAULT_DURATION_SEC 30
#define MAX_THREADS          500
#define MAX_POOL_SIZE        500
#define LATENCY_BUCKETS      5000
#define CHECKOUT_TIMEOUT_MS  5000

// Pool slot states
#define SLOT_FREE    0
#define SLOT_IN_USE  1
#define SLOT_IDLE    2   // only used in "idle" mode

// Pool modes
typedef enum {
    MODE_THREAD = 0,
    MODE_BORROW = 1,
    MODE_IDLE   = 2,
} pool_mode_t;

// ============================================================================
// Pool
// ============================================================================

typedef struct {
    PGconn      *conn;
    _Atomic int  state;       // SLOT_FREE / SLOT_IN_USE / SLOT_IDLE
    _Atomic int  last_owner;  // thread_id of last owner (-1 = none)
} pool_slot_t;

static pool_slot_t *g_pool      = NULL;
static int          g_pool_size = 0;
static pool_mode_t  g_mode      = MODE_THREAD;

// ============================================================================
// Shared state
// ============================================================================

static _Atomic int      g_running        = 1;
static _Atomic uint64_t g_total_ops      = 0;
static _Atomic uint64_t g_pool_exhausted = 0;
static _Atomic uint64_t g_query_errors   = 0;
static _Atomic uint64_t g_slot_reuses    = 0;  // idle mode: fast-path hits (same thread reclaim)
static _Atomic uint64_t g_slot_steals    = 0;  // idle mode: steals from other thread

// Per-thread stats
typedef struct {
    uint64_t ops_ok;
    uint64_t ops_pool_timeout;
    uint64_t ops_query_err;
    uint64_t latency_us[LATENCY_BUCKETS];
    int      latency_count;
    int      thread_id;
    int      is_scanner;
    uint64_t fast_path_hits;   // idle mode
    uint64_t steals;           // idle mode
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

static uint64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000ULL + ts.tv_nsec / 1000000ULL;
}

static void record_latency(thread_stats_t *st, uint64_t us) {
    if (st->latency_count < LATENCY_BUCKETS)
        st->latency_us[st->latency_count++] = us;
}

// ============================================================================
// Pool operations — MODE_THREAD
// ============================================================================

// Acquire once at thread start. Returns slot index or -1.
static int pool_acquire_thread_mode(void) {
    uint64_t deadline = now_ms() + CHECKOUT_TIMEOUT_MS;

    while (1) {
        for (int i = 0; i < g_pool_size; i++) {
            int expected = SLOT_FREE;
            if (atomic_compare_exchange_weak(&g_pool[i].state, &expected, SLOT_IN_USE)) {
                return i;
            }
        }
        if (now_ms() >= deadline) return -1;
        usleep(10000);
    }
}

// ============================================================================
// Pool operations — MODE_BORROW
// ============================================================================

static int pool_acquire_borrow(void) {
    uint64_t deadline = now_ms() + CHECKOUT_TIMEOUT_MS;

    while (1) {
        for (int i = 0; i < g_pool_size; i++) {
            int expected = SLOT_FREE;
            if (atomic_compare_exchange_weak(&g_pool[i].state, &expected, SLOT_IN_USE)) {
                return i;
            }
        }
        if (now_ms() >= deadline) return -1;
        usleep(10000);
    }
}

static void pool_release_borrow(int slot) {
    atomic_store(&g_pool[slot].state, SLOT_FREE);
}

// ============================================================================
// Pool operations — MODE_IDLE
// ============================================================================

// Per-thread consecutive fast-path counter (fairness mechanism)
#define FAIRNESS_YIELD_INTERVAL 4
static _Atomic int g_thread_consecutive_fp[MAX_THREADS];

// Returns slot index, sets *was_fast_path and *was_steal.
static int pool_acquire_idle(int thread_id, int *was_fast_path, int *was_steal) {
    *was_fast_path = 0;
    *was_steal = 0;

    uint64_t deadline = now_ms() + CHECKOUT_TIMEOUT_MS;

    while (1) {
        // Phase 1: fast path — find MY idle slot (O(1)-ish, no contention)
        // This simulates the TLS fast path in the real shim: if this thread's
        // own slot is idle, reclaim it instantly without scanning.
        for (int i = 0; i < g_pool_size; i++) {
            if (atomic_load(&g_pool[i].last_owner) != thread_id) continue;
            int expected = SLOT_IDLE;
            if (atomic_compare_exchange_weak(&g_pool[i].state, &expected, SLOT_IN_USE)) {
                *was_fast_path = 1;
                atomic_fetch_add(&g_thread_consecutive_fp[thread_id], 1);
                return i;
            }
        }

        // Phase 2: claim any FREE slot first (cheapest — no owner displacement)
        for (int i = 0; i < g_pool_size; i++) {
            int expected = SLOT_FREE;
            if (atomic_compare_exchange_weak(&g_pool[i].state, &expected, SLOT_IN_USE)) {
                atomic_store(&g_pool[i].last_owner, thread_id);
                atomic_store(&g_thread_consecutive_fp[thread_id], 0);
                return i;
            }
        }

        // Phase 3: steal any IDLE slot from another thread
        // Only reached when no FREE slots exist — this is the pressure-relief valve.
        for (int i = 0; i < g_pool_size; i++) {
            int expected = SLOT_IDLE;
            if (atomic_compare_exchange_weak(&g_pool[i].state, &expected, SLOT_IN_USE)) {
                atomic_store(&g_pool[i].last_owner, thread_id);
                atomic_store(&g_thread_consecutive_fp[thread_id], 0);
                *was_steal = 1;
                return i;
            }
        }

        if (now_ms() >= deadline) return -1;
        usleep(10000);
    }
}

static void pool_release_idle(int slot, int thread_id) {
    int fp_count = atomic_load(&g_thread_consecutive_fp[thread_id]);
    if (fp_count >= FAIRNESS_YIELD_INTERVAL) {
        // Yield: release as FREE so others can claim it
        atomic_store(&g_thread_consecutive_fp[thread_id], 0);
        atomic_store(&g_pool[slot].state, SLOT_FREE);
    } else {
        // Normal: release as IDLE with owner tracking
        atomic_store(&g_pool[slot].last_owner, thread_id);
        atomic_store(&g_pool[slot].state, SLOT_IDLE);
    }
}

// ============================================================================
// Workloads
// ============================================================================

static const char *scanner_queries[] = {
    "SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 1",
    "SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 4",
    "SELECT COUNT(*) FROM media_items WHERE deleted_at IS NULL",
    "SELECT COUNT(*) FROM media_parts WHERE deleted_at IS NULL",
    "SELECT id, title, metadata_type, added_at FROM metadata_items "
    "  WHERE metadata_type IN (1,4) ORDER BY id LIMIT 50",
    "SELECT id, directory_id, file FROM media_parts ORDER BY id LIMIT 50",
    "SELECT mi.id, mi.title, mp.file FROM metadata_items mi "
    "  JOIN media_items mitem ON mitem.metadata_item_id = mi.id "
    "  JOIN media_parts mp ON mp.media_item_id = mitem.id "
    "  WHERE mi.metadata_type = 1 LIMIT 20",
    "SELECT COUNT(*) FROM taggings",
    "SELECT tag_id, metadata_item_id FROM taggings ORDER BY id LIMIT 100",
    "SELECT id, title, originally_available_at, rating FROM metadata_items "
    "  ORDER BY updated_at DESC LIMIT 50",
};
static const int scanner_query_count = (int)(sizeof(scanner_queries) / sizeof(scanner_queries[0]));

static void run_query(PGconn *conn, int is_scanner, int thread_id, int *ok) {
    if (is_scanner) {
        const char *sql = scanner_queries[rand() % scanner_query_count];
        PGresult *res = PQexec(conn, sql);
        *ok = (PQresultStatus(res) == PGRES_TUPLES_OK);
        PQclear(res);
    } else {
        char sql[512];
        long base = 1000 + (long)(thread_id * 500);
        snprintf(sql, sizeof(sql),
            "SELECT id, title, originally_available_at, rating, summary "
            "FROM metadata_items WHERE id BETWEEN %ld AND %ld "
            "ORDER BY id LIMIT 10", base, base + 100);
        PGresult *res = PQexec(conn, sql);
        *ok = (PQresultStatus(res) == PGRES_TUPLES_OK);
        PQclear(res);
    }
}

// ============================================================================
// Thread functions
// ============================================================================

typedef struct {
    int thread_id;
    int is_scanner;
} thread_arg_t;

// --- MODE_THREAD: hold slot for entire lifetime ---
static void *thread_mode_thread(void *arg) {
    thread_arg_t  *ta  = (thread_arg_t *)arg;
    int            tid = ta->thread_id;
    thread_stats_t *st = &g_stats[tid];
    st->thread_id  = tid;
    st->is_scanner = ta->is_scanner;

    // Acquire once at start
    int slot = pool_acquire_thread_mode();
    if (slot < 0) {
        // Permanently locked out — count every missed op as timeout
        while (atomic_load(&g_running)) {
            st->ops_pool_timeout++;
            atomic_fetch_add(&g_pool_exhausted, 1);
            atomic_fetch_add(&g_total_ops, 1);
            // Record a 5s latency (the timeout duration)
            record_latency(st, CHECKOUT_TIMEOUT_MS * 1000ULL);
            // Sleep to avoid spinning — in reality the thread would be blocked
            usleep(100000);  // 100ms
        }
        return NULL;
    }

    PGconn *conn = g_pool[slot].conn;

    while (atomic_load(&g_running)) {
        uint64_t t0 = now_us();

        int query_ok = 0;
        run_query(conn, ta->is_scanner, tid, &query_ok);

        uint64_t dt = now_us() - t0;

        if (query_ok) { st->ops_ok++; } else { st->ops_query_err++; atomic_fetch_add(&g_query_errors, 1); }
        record_latency(st, dt);
        atomic_fetch_add(&g_total_ops, 1);

        if (!ta->is_scanner) usleep(500 + (uint32_t)(rand() % 1500));
    }

    atomic_store(&g_pool[slot].state, SLOT_FREE);
    return NULL;
}

// --- MODE_BORROW: acquire/release per query ---
static void *borrow_mode_thread(void *arg) {
    thread_arg_t  *ta  = (thread_arg_t *)arg;
    int            tid = ta->thread_id;
    thread_stats_t *st = &g_stats[tid];
    st->thread_id  = tid;
    st->is_scanner = ta->is_scanner;

    while (atomic_load(&g_running)) {
        uint64_t t0 = now_us();

        int slot = pool_acquire_borrow();
        if (slot < 0) {
            st->ops_pool_timeout++;
            atomic_fetch_add(&g_pool_exhausted, 1);
            atomic_fetch_add(&g_total_ops, 1);
            record_latency(st, now_us() - t0);
            continue;
        }

        int query_ok = 0;
        run_query(g_pool[slot].conn, ta->is_scanner, tid, &query_ok);

        pool_release_borrow(slot);

        uint64_t dt = now_us() - t0;

        if (query_ok) { st->ops_ok++; } else { st->ops_query_err++; atomic_fetch_add(&g_query_errors, 1); }
        record_latency(st, dt);
        atomic_fetch_add(&g_total_ops, 1);

        if (!ta->is_scanner) usleep(500 + (uint32_t)(rand() % 1500));
    }

    return NULL;
}

// --- MODE_IDLE: acquire/release-to-idle per query, with fast path ---
static void *idle_mode_thread(void *arg) {
    thread_arg_t  *ta  = (thread_arg_t *)arg;
    int            tid = ta->thread_id;
    thread_stats_t *st = &g_stats[tid];
    st->thread_id  = tid;
    st->is_scanner = ta->is_scanner;

    while (atomic_load(&g_running)) {
        uint64_t t0 = now_us();

        int was_fast_path = 0, was_steal = 0;
        int slot = pool_acquire_idle(tid, &was_fast_path, &was_steal);

        if (slot < 0) {
            st->ops_pool_timeout++;
            atomic_fetch_add(&g_pool_exhausted, 1);
            atomic_fetch_add(&g_total_ops, 1);
            record_latency(st, now_us() - t0);
            continue;
        }

        if (was_fast_path) { st->fast_path_hits++; atomic_fetch_add(&g_slot_reuses, 1); }
        if (was_steal)     { st->steals++;          atomic_fetch_add(&g_slot_steals, 1); }

        int query_ok = 0;
        run_query(g_pool[slot].conn, ta->is_scanner, tid, &query_ok);

        pool_release_idle(slot, tid);

        uint64_t dt = now_us() - t0;

        if (query_ok) { st->ops_ok++; } else { st->ops_query_err++; atomic_fetch_add(&g_query_errors, 1); }
        record_latency(st, dt);
        atomic_fetch_add(&g_total_ops, 1);

        if (!ta->is_scanner) usleep(500 + (uint32_t)(rand() % 1500));
    }

    return NULL;
}

// ============================================================================
// Stats
// ============================================================================

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}

static uint64_t pct(uint64_t *arr, int n, double p) {
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
            g_pool_size = i;
            printf("(got %d)\n", i);
            return 0;
        }
        char sp[256];
        snprintf(sp, sizeof(sp), "SET search_path TO %s", g_pg_schema);
        PGresult *r = PQexec(g_pool[i].conn, sp);
        PQclear(r);
        atomic_store(&g_pool[i].state, SLOT_FREE);
        atomic_store(&g_pool[i].last_owner, -1);

        if ((i + 1) % 10 == 0) { printf("%d.. ", i + 1); fflush(stdout); }
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
    if (argc < 2) {
        fprintf(stderr, "Usage: %s <thread|borrow|idle> [pool_size] [threads] [duration_sec]\n", argv[0]);
        return 1;
    }

    // Parse mode
    const char *mode_str = argv[1];
    if      (strcmp(mode_str, "thread") == 0) g_mode = MODE_THREAD;
    else if (strcmp(mode_str, "borrow") == 0) g_mode = MODE_BORROW;
    else if (strcmp(mode_str, "idle")   == 0) g_mode = MODE_IDLE;
    else {
        fprintf(stderr, "Unknown mode: %s (use thread, borrow, or idle)\n", mode_str);
        return 1;
    }

    int pool_size    = DEFAULT_POOL_SIZE;
    int num_threads  = DEFAULT_THREADS;
    int duration_sec = DEFAULT_DURATION_SEC;

    if (argc >= 3) pool_size    = atoi(argv[2]);
    if (argc >= 4) num_threads  = atoi(argv[3]);
    if (argc >= 5) duration_sec = atoi(argv[4]);

    if (pool_size    < 1)             pool_size    = 1;
    if (pool_size    > MAX_POOL_SIZE) pool_size    = MAX_POOL_SIZE;
    if (num_threads  < 1)             num_threads  = 1;
    if (num_threads  > MAX_THREADS)   num_threads  = MAX_THREADS;
    if (duration_sec < 1)             duration_sec = 1;

    g_pool_size            = pool_size;
    g_num_threads          = num_threads;
    g_duration_sec         = duration_sec;
    g_configured_pool_size = pool_size;

    g_pg_host     = getenv("PLEX_PG_HOST");     if (!g_pg_host)     g_pg_host     = "/tmp";
    g_pg_database = getenv("PLEX_PG_DATABASE"); if (!g_pg_database) g_pg_database = "plex_stress";
    g_pg_user     = getenv("PLEX_PG_USER");     if (!g_pg_user)     g_pg_user     = "plex";
    g_pg_password = getenv("PLEX_PG_PASSWORD");
    g_pg_schema   = getenv("PLEX_PG_SCHEMA");   if (!g_pg_schema)   g_pg_schema   = "plex";

    const char *mode_labels[] = {"thread (current shim)", "borrow (classic pool)", "idle (SLOT_IDLE proposal)"};

    printf("\n\033[1m=== Pool Mode Comparison: %s ===\033[0m\n", mode_labels[g_mode]);
    printf("  Mode:     %s\n", mode_str);
    printf("  Pool:     %d connections\n", pool_size);
    printf("  Threads:  %d\n", num_threads);
    printf("  Duration: %ds\n", duration_sec);
    printf("  Database: %s @ %s\n", g_pg_database, g_pg_host);
    printf("  Timeout:  %dms\n\n", CHECKOUT_TIMEOUT_MS);

    if (pool_init() < 0) return 1;
    if (g_pool_size == 0) { fprintf(stderr, "No connections.\n"); return 1; }

    // Verify connectivity
    printf("  Verifying... ");
    fflush(stdout);
    PGresult *r = PQexec(g_pool[0].conn, "SELECT COUNT(*) FROM metadata_items");
    if (PQresultStatus(r) == PGRES_TUPLES_OK) {
        printf("OK (%s rows)\n\n", PQgetvalue(r, 0, 0));
    } else {
        printf("WARN: %s\n\n", PQresultErrorMessage(r));
    }
    PQclear(r);

    g_stats = calloc((size_t)num_threads, sizeof(thread_stats_t));
    pthread_t    *threads = calloc((size_t)num_threads, sizeof(pthread_t));
    thread_arg_t *args    = calloc((size_t)num_threads, sizeof(thread_arg_t));

    int stream_threads = 4;
    for (int i = 0; i < num_threads; i++) {
        args[i].thread_id  = i;
        args[i].is_scanner = (i >= stream_threads) ? 1 : 0;
    }

    printf("  Starting %d stream + %d scanner threads...\n\n", stream_threads, num_threads - stream_threads);

    void *(*thread_fn)(void *) = NULL;
    switch (g_mode) {
        case MODE_THREAD: thread_fn = thread_mode_thread; break;
        case MODE_BORROW: thread_fn = borrow_mode_thread; break;
        case MODE_IDLE:   thread_fn = idle_mode_thread;   break;
    }

    for (int i = 0; i < num_threads; i++) {
        pthread_create(&threads[i], NULL, thread_fn, &args[i]);
    }

    uint64_t t_start = now_us();
    for (int elapsed = 0; elapsed < duration_sec; elapsed++) {
        sleep(1);
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
    uint64_t total_fast_path = 0, total_steals = 0;
    size_t lat_cap = (size_t)num_threads * LATENCY_BUCKETS;
    uint64_t *all_lat = malloc(lat_cap * sizeof(uint64_t));
    int all_lat_n = 0;

    for (int i = 0; i < num_threads; i++) {
        total_ok   += g_stats[i].ops_ok;
        total_exh  += g_stats[i].ops_pool_timeout;
        total_qerr += g_stats[i].ops_query_err;
        total_fast_path += g_stats[i].fast_path_hits;
        total_steals    += g_stats[i].steals;
        int n = g_stats[i].latency_count;
        if (all_lat_n + n <= (int)lat_cap) {
            memcpy(&all_lat[all_lat_n], g_stats[i].latency_us, (size_t)n * sizeof(uint64_t));
            all_lat_n += n;
        }
    }

    uint64_t total_ops = total_ok + total_exh + total_qerr;
    double err_pct = total_ops > 0 ? 100.0 * (double)(total_exh + total_qerr) / (double)total_ops : 0.0;
    double exh_pct = total_ops > 0 ? 100.0 * (double)total_exh / (double)total_ops : 0.0;

    qsort(all_lat, (size_t)all_lat_n, sizeof(uint64_t), cmp_u64);

    printf("\033[1m=== Results: %s ===\033[0m\n", mode_labels[g_mode]);
    printf("  Pool size:       %d\n", g_configured_pool_size);
    printf("  Threads:         %d\n", num_threads);
    printf("  Duration:        %.1fs\n\n", elapsed_sec);
    printf("  Total ops:       %llu\n",  (unsigned long long)total_ops);
    printf("  Ops/sec:         %.1f\n",  (double)total_ops / elapsed_sec);
    printf("  Successes:       %llu\n",  (unsigned long long)total_ok);
    printf("  Pool timeouts:   %llu (%.2f%%)\n", (unsigned long long)total_exh, exh_pct);
    printf("  Query errors:    %llu\n",  (unsigned long long)total_qerr);
    printf("  Error rate:      %.2f%%\n\n", err_pct);

    if (g_mode == MODE_IDLE) {
        uint64_t total_acquires = total_ok + total_qerr;  // successful acquires
        double reuse_pct = total_acquires > 0 ? 100.0 * (double)total_fast_path / (double)total_acquires : 0.0;
        double steal_pct = total_acquires > 0 ? 100.0 * (double)total_steals / (double)total_acquires : 0.0;
        printf("  SLOT_IDLE stats:\n");
        printf("    Fast-path reuse:  %llu (%.1f%% of acquires)\n",
               (unsigned long long)total_fast_path, reuse_pct);
        printf("    Steals:           %llu (%.1f%% of acquires)\n",
               (unsigned long long)total_steals, steal_pct);
        printf("\n");
    }

    if (all_lat_n > 0) {
        printf("  Latency (ms):\n");
        printf("    p50:  %.1f\n", (double)pct(all_lat, all_lat_n, 50.0)  / 1000.0);
        printf("    p90:  %.1f\n", (double)pct(all_lat, all_lat_n, 90.0)  / 1000.0);
        printf("    p95:  %.1f\n", (double)pct(all_lat, all_lat_n, 95.0)  / 1000.0);
        printf("    p99:  %.1f\n", (double)pct(all_lat, all_lat_n, 99.0)  / 1000.0);
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
    printf("    STREAM   ok=%-8llu  pool_timeout=%llu\n",
           (unsigned long long)stream_ok, (unsigned long long)stream_exh);
    printf("    SCANNER  ok=%-8llu  pool_timeout=%llu\n",
           (unsigned long long)scanner_ok, (unsigned long long)scanner_exh);
    printf("\n");

    // Verdict
    int passed = (err_pct < 1.0);
    if (passed) {
        printf("\033[32m  RESULT: PASS (error rate %.2f%% < 1%%)\033[0m\n\n", err_pct);
    } else {
        printf("\033[31m  RESULT: FAIL (error rate %.2f%% >= 1%%)\033[0m\n\n", err_pct);
    }

    free(all_lat);
    free(g_stats);
    free(threads);
    free(args);
    return passed ? 0 : 1;
}
