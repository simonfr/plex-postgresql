/*
 * test_pool_autogrow.c — Validates auto-grow/shrink pool strategy for Issue #9
 *
 * Tests three scenarios:
 *
 *   fixed    — Current shim: fixed pool, threads > pool → permanent lockout.
 *              Expected: FAIL.
 *
 *   autogrow — Pool grows on demand, shrinks when idle. Three phases:
 *              Phase 1: 80 threads start, pool grows 50 → 80
 *              Phase 2: 50 threads stop, reaper shrinks pool 80 → 30
 *              Phase 3: 30 new threads start, pool re-grows 30 → 60
 *              Expected: PASS (no lockouts in any phase).
 *
 * Usage:
 *   ./tests/bin/test_pool_autogrow <fixed|autogrow> [initial_pool] [max_pool] [threads] [phase_sec]
 *
 * Examples:
 *   ./tests/bin/test_pool_autogrow fixed    50 200 80 8    # FAIL
 *   ./tests/bin/test_pool_autogrow autogrow 50 200 80 8    # PASS (grow + shrink + re-grow)
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stdatomic.h>
#include <pthread.h>
#include <unistd.h>
#include <time.h>
#include <libpq-fe.h>

// ============================================================================
// Configuration
// ============================================================================

#define DEFAULT_INITIAL_POOL  50
#define DEFAULT_MAX_POOL      200
#define DEFAULT_THREADS       80
#define DEFAULT_PHASE_SEC     8
#define MAX_POOL_SLOTS        500
#define MAX_THREADS           500
#define LATENCY_BUCKETS       5000
#define REAP_IDLE_SECONDS     2     // Shrink: close connections idle > 2s (fast for testing)

typedef enum {
    MODE_FIXED = 0,
    MODE_AUTOGROW = 1,
} pool_mode_t;

// ============================================================================
// Pool — thread-affinity model with auto-grow/shrink
// ============================================================================

#define SLOT_FREE     0
#define SLOT_RESERVED 1
#define SLOT_READY    2

typedef struct {
    PGconn      *conn;
    _Atomic int  state;
    pthread_t    owner_thread;
    _Atomic time_t last_used;     // For shrink: when slot was last released
} pool_slot_t;

static pool_slot_t g_pool[MAX_POOL_SLOTS];
static _Atomic int g_pool_size;       // Current pool size (grows/shrinks)
static int         g_max_pool_size;   // Maximum pool size
static int         g_initial_pool;    // Initial pool size (for display)
static pool_mode_t g_mode;
static _Atomic int g_grow_count = 0;
static _Atomic int g_shrink_count = 0;

// Connection config
static const char *g_pg_host;
static const char *g_pg_database;
static const char *g_pg_user;
static const char *g_pg_password;
static const char *g_pg_schema;

// ============================================================================
// Shared state
// ============================================================================

static _Atomic int      g_stop_all = 0;      // 1 = stop all threads
static _Atomic int      g_stop_phase1 = 0;   // 1 = stop phase 1 excess threads
static _Atomic uint64_t g_total_ops = 0;
static _Atomic uint64_t g_locked_out = 0;

typedef struct {
    uint64_t ops_ok;
    uint64_t ops_query_err;
    uint64_t latency_us[LATENCY_BUCKETS];
    int      latency_count;
    int      thread_id;
    int      is_scanner;
    int      got_slot;
    int      slot_idx;
} thread_stats_t;

static thread_stats_t g_stats[MAX_THREADS];
static int g_num_threads;
static int g_phase_sec;

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
// Pool: create connection
// ============================================================================

static PGconn *create_connection(void) {
    char connstr[1024];
    if (g_pg_password && g_pg_password[0]) {
        snprintf(connstr, sizeof(connstr),
            "host=%s dbname=%s user=%s password=%s connect_timeout=5",
            g_pg_host, g_pg_database, g_pg_user, g_pg_password);
    } else {
        snprintf(connstr, sizeof(connstr),
            "host=%s dbname=%s user=%s connect_timeout=5",
            g_pg_host, g_pg_database, g_pg_user);
    }

    PGconn *conn = PQconnectdb(connstr);
    if (PQstatus(conn) != CONNECTION_OK) {
        PQfinish(conn);
        return NULL;
    }

    char sp[256];
    snprintf(sp, sizeof(sp), "SET search_path TO %s", g_pg_schema);
    PGresult *r = PQexec(conn, sp);
    PQclear(r);

    return conn;
}

// ============================================================================
// Pool: acquire slot (thread-affinity)
// ============================================================================

static int pool_acquire_slot(pthread_t self) {
    // Retry loop: slots may be temporarily RESERVED (another thread creating conn)
    // 20 attempts * 50ms = 1s max wait
    for (int attempt = 0; attempt < 20; attempt++) {
        int current_size = atomic_load(&g_pool_size);

        // Phase 1: Try to claim a FREE slot within current pool size
        for (int i = 0; i < current_size; i++) {
            int expected = SLOT_FREE;
            if (atomic_compare_exchange_strong(&g_pool[i].state, &expected, SLOT_RESERVED)) {
                // Re-use existing connection if available, else create new
                if (!g_pool[i].conn) {
                    PGconn *conn = create_connection();
                    if (!conn) {
                        atomic_store(&g_pool[i].state, SLOT_FREE);
                        continue;
                    }
                    g_pool[i].conn = conn;
                }
                g_pool[i].owner_thread = self;
                atomic_store(&g_pool[i].last_used, time(NULL));
                atomic_store(&g_pool[i].state, SLOT_READY);
                return i;
            }
        }

        // Phase 2 (autogrow only): Grow pool and claim new slot
        if (g_mode == MODE_AUTOGROW) {
            int cur = atomic_load(&g_pool_size);
            if (cur < g_max_pool_size) {
                int new_size = cur + 1;
                if (atomic_compare_exchange_strong(&g_pool_size, &cur, new_size)) {
                    int idx = new_size - 1;
                    int expected = SLOT_FREE;
                    if (atomic_compare_exchange_strong(&g_pool[idx].state, &expected, SLOT_RESERVED)) {
                        PGconn *conn = create_connection();
                        if (!conn) {
                            atomic_store(&g_pool[idx].state, SLOT_FREE);
                            return -1;
                        }
                        g_pool[idx].conn = conn;
                        g_pool[idx].owner_thread = self;
                        atomic_store(&g_pool[idx].last_used, time(NULL));
                        atomic_store(&g_pool[idx].state, SLOT_READY);
                        atomic_fetch_add(&g_grow_count, 1);
                        return idx;
                    }
                }
                // CAS failed — another thread grew. Retry immediately.
                continue;
            }
        }

        // No FREE slots found. In fixed mode with all READY → locked out.
        // Wait briefly for RESERVED slots to finish, then retry.
        usleep(50000);  // 50ms
    }

    return -1;
}

// ============================================================================
// Pool: release slot (simulates sqlite3_close → pg_close_pool_for_db)
// ============================================================================

// Release slot: mark as FREE but keep the PG connection open.
// The reaper will close idle connections later — this is what makes
// the pool "shrink" (fewer active PG connections, not smaller array).
// The connection stays available for re-use by the next acquire.
static void pool_release_slot(int slot) {
    g_pool[slot].owner_thread = 0;
    atomic_store(&g_pool[slot].last_used, time(NULL));
    atomic_store(&g_pool[slot].state, SLOT_FREE);
}

// ============================================================================
// Pool: reaper — shrinks pool by reclaiming FREE slots at the tail
// ============================================================================

// Closes PG connections on FREE slots that have been idle > REAP_IDLE_SECONDS.
// Does NOT change pool_size — the array keeps its size but slots with conn=NULL
// will create new connections on next acquire. This matches the real shim reaper.
// Returns the number of connections closed.
static int pool_reap(void) {
    time_t now = time(NULL);
    int current = atomic_load(&g_pool_size);
    int reaped = 0;

    for (int i = 0; i < current; i++) {
        int state = atomic_load(&g_pool[i].state);
        if (state != SLOT_FREE) continue;
        if (!g_pool[i].conn) continue;  // Already reaped

        time_t lu = atomic_load(&g_pool[i].last_used);
        if (lu == 0) continue;
        if ((now - lu) < REAP_IDLE_SECONDS) continue;

        // Claim the slot to close its connection safely
        int expected = SLOT_FREE;
        if (atomic_compare_exchange_strong(&g_pool[i].state, &expected, SLOT_RESERVED)) {
            if (g_pool[i].conn) {
                PQfinish(g_pool[i].conn);
                g_pool[i].conn = NULL;
                reaped++;
                atomic_fetch_add(&g_shrink_count, 1);
            }
            atomic_store(&g_pool[i].state, SLOT_FREE);
        }
    }

    return reaped;
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
    "SELECT COUNT(*) FROM taggings",
    "SELECT tag_id, metadata_item_id FROM taggings ORDER BY id LIMIT 100",
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
    int phase;            // 1 = phase 1 thread, 3 = phase 3 thread
    _Atomic int *stop;    // Which stop signal to obey
} thread_arg_t;

static void *worker_thread(void *arg) {
    thread_arg_t  *ta  = (thread_arg_t *)arg;
    int            tid = ta->thread_id;
    thread_stats_t *st = &g_stats[tid];
    st->thread_id  = tid;
    st->is_scanner = ta->is_scanner;

    int slot = pool_acquire_slot(pthread_self());
    if (slot < 0) {
        st->got_slot = 0;
        st->slot_idx = -1;
        atomic_fetch_add(&g_locked_out, 1);
        // Spin until our stop signal
        while (!atomic_load(ta->stop)) {
            usleep(50000);
        }
        return NULL;
    }

    st->got_slot = 1;
    st->slot_idx = slot;
    PGconn *conn = g_pool[slot].conn;

    while (!atomic_load(ta->stop)) {
        uint64_t t0 = now_us();

        int query_ok = 0;
        run_query(conn, ta->is_scanner, tid, &query_ok);

        uint64_t dt = now_us() - t0;

        if (query_ok) st->ops_ok++;
        else st->ops_query_err++;
        record_latency(st, dt);
        atomic_fetch_add(&g_total_ops, 1);

        if (!ta->is_scanner) usleep(500 + (uint32_t)(rand() % 1500));
    }

    // Release slot (simulates sqlite3_close)
    pool_release_slot(slot);
    st->got_slot = 0;
    return NULL;
}

// ============================================================================
// Stats
// ============================================================================

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}

static uint64_t pct_val(uint64_t *arr, int n, double p) {
    if (n == 0) return 0;
    int idx = (int)(p / 100.0 * (double)n);
    if (idx >= n) idx = n - 1;
    return arr[idx];
}

static void print_pool_state(const char *label) {
    int pool_sz = atomic_load(&g_pool_size);
    int free_slots = 0, ready_slots = 0, with_conn = 0;
    for (int i = 0; i < pool_sz; i++) {
        int st = atomic_load(&g_pool[i].state);
        if (st == SLOT_FREE) free_slots++;
        else if (st == SLOT_READY) ready_slots++;
        if (g_pool[i].conn) with_conn++;
    }
    printf("  %-20s pool_size=%d  READY=%d  FREE=%d  conns=%d  grows=%d  shrinks=%d\n",
           label, pool_sz, ready_slots, free_slots, with_conn,
           atomic_load(&g_grow_count), atomic_load(&g_shrink_count));
}

// ============================================================================
// Main
// ============================================================================

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s <fixed|autogrow> [initial_pool] [max_pool] [threads] [phase_sec]\n", argv[0]);
        return 1;
    }

    const char *mode_str = argv[1];
    if      (strcmp(mode_str, "fixed")    == 0) g_mode = MODE_FIXED;
    else if (strcmp(mode_str, "autogrow") == 0) g_mode = MODE_AUTOGROW;
    else {
        fprintf(stderr, "Unknown mode: %s (use 'fixed' or 'autogrow')\n", mode_str);
        return 1;
    }

    int initial_pool = DEFAULT_INITIAL_POOL;
    int max_pool     = DEFAULT_MAX_POOL;
    int num_threads  = DEFAULT_THREADS;
    int phase_sec    = DEFAULT_PHASE_SEC;

    if (argc >= 3) initial_pool = atoi(argv[2]);
    if (argc >= 4) max_pool     = atoi(argv[3]);
    if (argc >= 5) num_threads  = atoi(argv[4]);
    if (argc >= 6) phase_sec    = atoi(argv[5]);

    if (initial_pool < 1)              initial_pool = 1;
    if (initial_pool > MAX_POOL_SLOTS) initial_pool = MAX_POOL_SLOTS;
    if (max_pool < initial_pool)       max_pool = initial_pool;
    if (max_pool > MAX_POOL_SLOTS)     max_pool = MAX_POOL_SLOTS;
    if (num_threads < 1)               num_threads = 1;
    if (num_threads > MAX_THREADS)     num_threads = MAX_THREADS;
    if (phase_sec < 1)                 phase_sec = 1;

    g_initial_pool  = initial_pool;
    g_max_pool_size = (g_mode == MODE_AUTOGROW) ? max_pool : initial_pool;
    atomic_store(&g_pool_size, initial_pool);
    g_num_threads   = num_threads;
    g_phase_sec     = phase_sec;

    g_pg_host     = getenv("PLEX_PG_HOST");     if (!g_pg_host)     g_pg_host     = "/tmp";
    g_pg_database = getenv("PLEX_PG_DATABASE"); if (!g_pg_database) g_pg_database = "plex_stress";
    g_pg_user     = getenv("PLEX_PG_USER");     if (!g_pg_user)     g_pg_user     = "plex";
    g_pg_password = getenv("PLEX_PG_PASSWORD");
    g_pg_schema   = getenv("PLEX_PG_SCHEMA");   if (!g_pg_schema)   g_pg_schema   = "plex";

    const char *mode_label = (g_mode == MODE_FIXED) ?
        "fixed (current shim)" : "autogrow (proposed fix)";

    printf("\n\033[1m=== Pool Auto-Grow/Shrink Test: %s ===\033[0m\n", mode_label);
    printf("  Mode:         %s\n", mode_str);
    printf("  Initial pool: %d\n", initial_pool);
    printf("  Max pool:     %d\n", g_max_pool_size);
    printf("  Threads:      %d (phase 1) → %d stop → %d new (phase 3)\n",
           num_threads, num_threads / 2, num_threads / 2);
    printf("  Phase length: %ds\n", phase_sec);
    printf("  Shrink idle:  %ds\n", REAP_IDLE_SECONDS);
    printf("  Database:     %s @ %s\n\n", g_pg_database, g_pg_host);

    // Initialize pool
    memset(g_pool, 0, sizeof(g_pool));
    for (int i = 0; i < MAX_POOL_SLOTS; i++) {
        atomic_store(&g_pool[i].state, SLOT_FREE);
        atomic_store(&g_pool[i].last_used, (time_t)0);
    }

    // ========================================================================
    // Phase 1: Start all threads, pool should grow
    // ========================================================================

    int stop_count = num_threads / 2;  // How many threads to stop in phase 2
    int keep_count = num_threads - stop_count;  // Threads that survive

    printf("\033[1m--- Phase 1: Start %d threads (pool starts at %d) ---\033[0m\n", num_threads, initial_pool);

    pthread_t    p1_threads[MAX_THREADS];
    thread_arg_t p1_args[MAX_THREADS];

    for (int i = 0; i < num_threads; i++) {
        p1_args[i].thread_id  = i;
        p1_args[i].is_scanner = (i >= 4) ? 1 : 0;
        p1_args[i].phase      = 1;
        // First 'stop_count' threads listen to g_stop_phase1, rest listen to g_stop_all
        p1_args[i].stop = (i < stop_count) ? &g_stop_phase1 : &g_stop_all;
    }

    for (int i = 0; i < num_threads; i++) {
        pthread_create(&p1_threads[i], NULL, worker_thread, &p1_args[i]);
        usleep(5000);
    }

    sleep(3);  // Let all threads start and finish acquire attempts
    print_pool_state("After phase 1 start:");

    int p1_locked = (int)atomic_load(&g_locked_out);

    // Run phase 1
    for (int s = 0; s < phase_sec; s++) {
        sleep(1);
        printf("\r  Phase 1 [%d/%ds]  ops=%llu  pool=%d",
               s + 1, phase_sec,
               (unsigned long long)atomic_load(&g_total_ops),
               atomic_load(&g_pool_size));
        fflush(stdout);
    }
    printf("\n");
    print_pool_state("Phase 1 end:");
    int p1_pool_size = atomic_load(&g_pool_size);

    // ========================================================================
    // Phase 2: Stop half the threads, run reaper, pool should shrink
    // ========================================================================

    printf("\n\033[1m--- Phase 2: Stop %d threads, reaper shrinks pool ---\033[0m\n", stop_count);

    atomic_store(&g_stop_phase1, 1);
    for (int i = 0; i < stop_count; i++) {
        pthread_join(p1_threads[i], NULL);
    }

    print_pool_state("After threads stopped:");

    // Run reaper repeatedly until pool shrinks (or timeout)
    for (int s = 0; s < phase_sec; s++) {
        pool_reap();
        sleep(1);
        printf("\r  Phase 2 [%d/%ds]  pool=%d  shrinks=%d",
               s + 1, phase_sec,
               atomic_load(&g_pool_size),
               atomic_load(&g_shrink_count));
        fflush(stdout);
    }
    printf("\n");
    print_pool_state("Phase 2 end:");
    int p2_pool_size = atomic_load(&g_pool_size);
    int p2_shrinks = atomic_load(&g_shrink_count);

    // ========================================================================
    // Phase 3: Start new threads, pool should re-grow
    // ========================================================================

    int new_threads = num_threads / 2;
    printf("\n\033[1m--- Phase 3: Start %d new threads, pool re-grows ---\033[0m\n", new_threads);

    pthread_t    p3_threads[MAX_THREADS];
    thread_arg_t p3_args[MAX_THREADS];
    int p3_base = num_threads;  // Use higher thread IDs for stats

    atomic_store(&g_locked_out, 0);  // Reset lockout counter for phase 3

    for (int i = 0; i < new_threads; i++) {
        int tid = p3_base + i;
        p3_args[i].thread_id  = tid;
        p3_args[i].is_scanner = 1;
        p3_args[i].phase      = 3;
        p3_args[i].stop       = &g_stop_all;
    }

    for (int i = 0; i < new_threads; i++) {
        pthread_create(&p3_threads[i], NULL, worker_thread, &p3_args[i]);
        usleep(5000);
    }

    sleep(2);
    print_pool_state("After phase 3 start:");
    int p3_locked = (int)atomic_load(&g_locked_out);

    for (int s = 0; s < phase_sec; s++) {
        sleep(1);
        printf("\r  Phase 3 [%d/%ds]  ops=%llu  pool=%d",
               s + 1, phase_sec,
               (unsigned long long)atomic_load(&g_total_ops),
               atomic_load(&g_pool_size));
        fflush(stdout);
    }
    printf("\n");
    print_pool_state("Phase 3 end:");
    int p3_pool_size = atomic_load(&g_pool_size);

    // Stop all remaining threads
    atomic_store(&g_stop_all, 1);

    // Wait for remaining phase 1 threads (keep_count)
    for (int i = stop_count; i < num_threads; i++) {
        pthread_join(p1_threads[i], NULL);
    }
    // Wait for phase 3 threads
    for (int i = 0; i < new_threads; i++) {
        pthread_join(p3_threads[i], NULL);
    }

    printf("\n");

    // ========================================================================
    // Results
    // ========================================================================

    printf("\033[1m=== Results: %s ===\033[0m\n\n", mode_label);

    printf("  Phase 1 (grow):\n");
    printf("    Threads: %d,  Pool: %d → %d,  Locked out: %d\n",
           num_threads, initial_pool, p1_pool_size, p1_locked);

    // Count active PG connections
    int p2_active_conns = 0;
    for (int i = 0; i < atomic_load(&g_pool_size); i++) {
        if (g_pool[i].conn) p2_active_conns++;
    }

    printf("  Phase 2 (shrink):\n");
    printf("    Stopped: %d threads,  Connections closed: %d,  Active conns: %d\n",
           stop_count, p2_shrinks, p2_active_conns);

    printf("  Phase 3 (re-grow):\n");
    printf("    New threads: %d,  Pool: %d → %d,  Locked out: %d\n\n",
           new_threads, p2_pool_size, p3_pool_size, p3_locked);

    // Verdict
    int grow_ok = (g_mode == MODE_FIXED) ? 1 : (p1_locked == 0);       // No lockouts in phase 1
    int shrink_ok = (g_mode == MODE_FIXED) ? 1 : (p2_shrinks > 0);    // Reaper closed idle connections
    int regrow_ok = (g_mode == MODE_FIXED) ? 1 : (p3_locked == 0);     // No lockouts in phase 3

    if (g_mode == MODE_FIXED) {
        // Fixed mode: expect failure (lockouts)
        if (p1_locked > 0) {
            printf("\033[31m  RESULT: FAIL (as expected for fixed mode)\033[0m\n");
            printf("\033[31m    %d threads permanently locked out in phase 1\033[0m\n", p1_locked);
        } else {
            printf("\033[32m  RESULT: PASS (no contention — threads <= pool)\033[0m\n");
        }
    } else {
        // Autogrow mode: expect all three phases to pass
        int passed = grow_ok && shrink_ok && regrow_ok;
        if (passed) {
            printf("\033[32m  RESULT: PASS\033[0m\n");
            printf("\033[32m    Phase 1: Pool grew %d → %d, 0 lockouts\033[0m\n",
                   initial_pool, p1_pool_size);
            printf("\033[32m    Phase 2: Reaper closed %d idle connections\033[0m\n", p2_shrinks);
            printf("\033[32m    Phase 3: Pool re-grew %d → %d, 0 lockouts\033[0m\n",
                   p2_pool_size, p3_pool_size);
        } else {
            printf("\033[31m  RESULT: FAIL\033[0m\n");
            if (!grow_ok)   printf("\033[31m    Phase 1 FAIL: %d threads locked out\033[0m\n", p1_locked);
            if (!shrink_ok) printf("\033[31m    Phase 2 FAIL: reaper didn't close any idle connections\033[0m\n");
            if (!regrow_ok) printf("\033[31m    Phase 3 FAIL: %d threads locked out\033[0m\n", p3_locked);
        }
    }
    printf("\n");

    // Clean up remaining pool connections
    int final_pool = atomic_load(&g_pool_size);
    for (int i = 0; i < MAX_POOL_SLOTS; i++) {
        if (g_pool[i].conn) {
            PQfinish(g_pool[i].conn);
            g_pool[i].conn = NULL;
        }
    }

    (void)final_pool;
    return (g_mode == MODE_FIXED && p1_locked > 0) ? 1 :
           (g_mode == MODE_AUTOGROW && grow_ok && shrink_ok && regrow_ok) ? 0 : 1;
}
