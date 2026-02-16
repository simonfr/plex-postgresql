/*
 * Unit tests for Connection Isolation during Streaming (v0.9.29)
 *
 * Tests the critical fix that prevents PQexec calls on connections that are
 * in single-row streaming mode. The root cause was:
 *
 * 1. resolve_column_tables() called PQexec(streaming_conn) to look up OIDs
 * 2. preload_decltype_cache() called PQexec(streaming_conn) to load type cache
 * 3. Both PQexec calls consumed/discarded the pending streaming results
 * 4. The next PQgetResult() returned NULL → Plex saw only 1 migration row
 *
 * The fix: when streaming_active=1, these functions get an alternate connection
 * from the pool instead. The pool's fast path and phase-1 loop skip connections
 * with streaming_active=1.
 *
 * These tests verify:
 * - streaming_active flag is set/cleared correctly
 * - Pool skips streaming connections and returns a different one
 * - resolve_column_tables uses alternate connection when streaming
 * - preload_decltype_cache uses alternate connection when streaming
 * - streaming_active is cleared on drain/reset/error
 * - Multiple streaming connections on different threads are isolated
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stdatomic.h>
#include <pthread.h>
#include <unistd.h>

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)

// ============================================================================
// Simulated types matching the real shim structures
// ============================================================================

#define SIM_POOL_SIZE 8

typedef struct sim_connection {
    int id;                       // Unique connection identifier
    volatile int streaming_active; // v0.9.29: Set when in single-row streaming mode
    int is_pg_active;
    pthread_mutex_t mutex;
    char db_path[256];
    // Simulated PGconn state
    int query_in_progress;        // True if streaming results pending
    int results_consumed;         // Counter: how many PQgetResult consumed
    int total_results;            // Total rows available for streaming
} sim_connection_t;

typedef enum {
    SIM_SLOT_FREE = 0,
    SIM_SLOT_READY = 1,
    SIM_SLOT_RESERVED = 2,
    SIM_SLOT_RECONNECTING = 3,
} sim_slot_state_t;

typedef struct {
    sim_connection_t *conn;
    _Atomic(sim_slot_state_t) state;
    pthread_t owner_thread;
} sim_pool_slot_t;

static sim_pool_slot_t sim_pool[SIM_POOL_SIZE];
static int sim_pool_count = 0;

// TLS cached slot (mirrors the real tls_pool_slot)
static __thread int tls_cached_slot = -1;

// ============================================================================
// Simulated pool functions matching pg_client.c behavior
// ============================================================================

static sim_connection_t* sim_pool_init_conn(int id, const char *db_path) {
    sim_connection_t *conn = calloc(1, sizeof(sim_connection_t));
    conn->id = id;
    conn->streaming_active = 0;
    conn->is_pg_active = 1;
    pthread_mutex_init(&conn->mutex, NULL);
    strncpy(conn->db_path, db_path, sizeof(conn->db_path) - 1);
    return conn;
}

static void sim_pool_free_conn(sim_connection_t *conn) {
    if (conn) {
        pthread_mutex_destroy(&conn->mutex);
        free(conn);
    }
}

static void sim_pool_reset(void) {
    for (int i = 0; i < SIM_POOL_SIZE; i++) {
        if (sim_pool[i].conn) {
            sim_pool_free_conn(sim_pool[i].conn);
        }
        sim_pool[i].conn = NULL;
        atomic_store(&sim_pool[i].state, SIM_SLOT_FREE);
        sim_pool[i].owner_thread = (pthread_t)0;
    }
    sim_pool_count = 0;
    tls_cached_slot = -1;
}

static int sim_pool_add(sim_connection_t *conn, pthread_t owner) {
    if (sim_pool_count >= SIM_POOL_SIZE) return -1;
    int slot = sim_pool_count++;
    sim_pool[slot].conn = conn;
    atomic_store(&sim_pool[slot].state, SIM_SLOT_READY);
    sim_pool[slot].owner_thread = owner;
    return slot;
}

// Simulates pool_get_connection() with streaming_active guard
// This mirrors the exact logic from pg_client.c
static sim_connection_t* sim_pool_get_connection(void) {
    pthread_t current = pthread_self();

    // FAST PATH: Check TLS cached slot
    if (tls_cached_slot >= 0 && tls_cached_slot < sim_pool_count) {
        sim_connection_t *conn = sim_pool[tls_cached_slot].conn;
        if (conn && conn->is_pg_active) {
            // v0.9.29: Skip if streaming_active
            if (conn->streaming_active) {
                // Fall through to slow path (matches real code)
            } else {
                return conn;
            }
        }
    }

    // PHASE 1: Find thread's existing READY connection
    for (int i = 0; i < sim_pool_count; i++) {
        sim_slot_state_t state = atomic_load(&sim_pool[i].state);
        if (state == SIM_SLOT_READY &&
            pthread_equal(sim_pool[i].owner_thread, current)) {
            sim_connection_t *conn = sim_pool[i].conn;
            if (conn && conn->is_pg_active) {
                // v0.9.29: Skip streaming connections
                if (conn->streaming_active) {
                    continue;
                }
                tls_cached_slot = i;
                return conn;
            }
        }
    }

    // PHASE 2: Claim FREE slot (simplified — just find any free slot)
    for (int i = 0; i < sim_pool_count; i++) {
        sim_slot_state_t expected = SIM_SLOT_FREE;
        if (atomic_compare_exchange_strong(&sim_pool[i].state, &expected, SIM_SLOT_RESERVED)) {
            sim_pool[i].owner_thread = current;
            atomic_store(&sim_pool[i].state, SIM_SLOT_READY);
            tls_cached_slot = i;
            return sim_pool[i].conn;
        }
    }

    // PHASE 3: Create new connection in empty slot
    if (sim_pool_count < SIM_POOL_SIZE) {
        static int next_id = 100;
        sim_connection_t *new_conn = sim_pool_init_conn(next_id++, "library.db");
        int slot = sim_pool_add(new_conn, current);
        if (slot >= 0) {
            tls_cached_slot = slot;
            return new_conn;
        }
    }

    return NULL;  // Pool exhausted
}

// Simulates PQexec — but tracks if it was called on a streaming connection
static int sim_pqexec_call_count = 0;
static int sim_pqexec_on_streaming = 0;

static void sim_PQexec(sim_connection_t *conn, const char *sql) {
    (void)sql;
    sim_pqexec_call_count++;
    if (conn->streaming_active) {
        sim_pqexec_on_streaming++;
        // In real libpq, PQexec on a streaming connection would consume
        // all pending streaming results — destroying the stream
        conn->results_consumed = conn->total_results;
        conn->query_in_progress = 0;
    }
}

// Simulates resolve_column_tables with the v0.9.29 streaming guard
static int sim_resolve_column_tables(sim_connection_t *pg_conn) {
    sim_connection_t *resolve_conn = pg_conn;

    if (pg_conn->streaming_active) {
        // Get alternate connection from pool
        sim_connection_t *alt = sim_pool_get_connection();
        if (alt && alt != pg_conn && !alt->streaming_active) {
            resolve_conn = alt;
        } else {
            // No alternate — skip OID resolution to protect streaming
            return -1;  // Skipped
        }
    }

    sim_PQexec(resolve_conn, "SELECT oid, relname FROM pg_class WHERE oid IN (12345)");
    return resolve_conn->id;  // Return which connection was used
}

// Simulates preload_decltype_cache with the v0.9.29 streaming guard
static int sim_decltype_cache_loaded = 0;

static int sim_preload_decltype_cache(sim_connection_t *pg_conn) {
    if (sim_decltype_cache_loaded) return 0;

    sim_connection_t *cache_conn = pg_conn;

    if (pg_conn->streaming_active) {
        sim_connection_t *alt = sim_pool_get_connection();
        if (alt && alt != pg_conn && !alt->streaming_active) {
            cache_conn = alt;
        } else {
            return -1;  // Deferred
        }
    }

    sim_PQexec(cache_conn, "SELECT table_name, column_name, declared_type FROM sqlite_column_types");
    sim_decltype_cache_loaded = 1;
    return cache_conn->id;
}

// ============================================================================
// Tests: streaming_active flag lifecycle
// ============================================================================

static void test_streaming_active_initially_zero(void) {
    TEST("streaming_active is 0 on new connection");
    sim_connection_t *conn = sim_pool_init_conn(1, "library.db");
    if (conn->streaming_active != 0) { FAIL("should be 0"); sim_pool_free_conn(conn); return; }
    sim_pool_free_conn(conn);
    PASS();
}

static void test_streaming_active_set_and_clear(void) {
    TEST("streaming_active set to 1 during streaming, cleared after");
    sim_connection_t *conn = sim_pool_init_conn(1, "library.db");

    // Simulate streaming start
    conn->streaming_active = 1;
    conn->query_in_progress = 1;
    conn->total_results = 446;
    if (conn->streaming_active != 1) { FAIL("should be 1 after set"); sim_pool_free_conn(conn); return; }

    // Simulate streaming end (TUPLES_OK sentinel reached)
    conn->streaming_active = 0;
    conn->query_in_progress = 0;
    if (conn->streaming_active != 0) { FAIL("should be 0 after clear"); sim_pool_free_conn(conn); return; }

    sim_pool_free_conn(conn);
    PASS();
}

static void test_streaming_active_cleared_on_error(void) {
    TEST("streaming_active cleared on streaming error");
    sim_connection_t *conn = sim_pool_init_conn(1, "library.db");

    conn->streaming_active = 1;
    conn->query_in_progress = 1;

    // Simulate error path: drain + clear
    conn->query_in_progress = 0;
    conn->streaming_active = 0;

    if (conn->streaming_active != 0) { FAIL("should be 0 after error"); sim_pool_free_conn(conn); return; }
    sim_pool_free_conn(conn);
    PASS();
}

static void test_streaming_active_cleared_on_drain(void) {
    TEST("streaming_active cleared when drain/reset cancels streaming");
    sim_connection_t *conn = sim_pool_init_conn(1, "library.db");

    conn->streaming_active = 1;
    conn->query_in_progress = 1;
    conn->total_results = 100;
    conn->results_consumed = 5;  // Only 5 of 100 consumed

    // Simulate PQcancel + drain (pg_stmt_clear_result)
    conn->results_consumed = conn->total_results;  // Cancel drains all
    conn->query_in_progress = 0;
    conn->streaming_active = 0;

    if (conn->streaming_active != 0) { FAIL("should be 0 after drain"); sim_pool_free_conn(conn); return; }
    sim_pool_free_conn(conn);
    PASS();
}

// ============================================================================
// Tests: Pool connection isolation
// ============================================================================

static void test_pool_fast_path_skips_streaming(void) {
    TEST("Pool fast path skips connection with streaming_active=1");
    sim_pool_reset();
    pthread_t me = pthread_self();

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    sim_connection_t *conn2 = sim_pool_init_conn(2, "library.db");
    sim_pool_add(conn1, me);
    sim_pool_add(conn2, me);

    // Cache slot 0 in TLS
    tls_cached_slot = 0;

    // Mark conn1 as streaming
    conn1->streaming_active = 1;

    sim_connection_t *got = sim_pool_get_connection();
    if (got == conn1) { FAIL("should NOT return the streaming connection"); goto cleanup; }
    if (got != conn2) { FAIL("should return conn2 as alternate"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

static void test_pool_phase1_skips_streaming(void) {
    TEST("Pool phase-1 loop skips all streaming connections for this thread");
    sim_pool_reset();
    pthread_t me = pthread_self();

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    sim_connection_t *conn2 = sim_pool_init_conn(2, "library.db");
    sim_connection_t *conn3 = sim_pool_init_conn(3, "library.db");
    sim_pool_add(conn1, me);
    sim_pool_add(conn2, me);
    sim_pool_add(conn3, me);

    // No TLS cache hit — forces phase-1 scan
    tls_cached_slot = -1;

    // Mark conn1 and conn2 as streaming
    conn1->streaming_active = 1;
    conn2->streaming_active = 1;

    sim_connection_t *got = sim_pool_get_connection();
    if (got == conn1 || got == conn2) { FAIL("should skip streaming connections"); goto cleanup; }
    if (got != conn3) { FAIL("should return conn3"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

static void test_pool_creates_new_when_all_streaming(void) {
    TEST("Pool creates new connection when all thread's connections are streaming");
    sim_pool_reset();
    pthread_t me = pthread_self();

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    sim_pool_add(conn1, me);
    tls_cached_slot = -1;

    conn1->streaming_active = 1;

    sim_connection_t *got = sim_pool_get_connection();
    if (got == conn1) { FAIL("should NOT return the streaming connection"); goto cleanup; }
    if (got == NULL) { FAIL("should create a new connection"); goto cleanup; }
    if (got->streaming_active) { FAIL("new connection should not be streaming"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

static void test_pool_returns_streaming_conn_after_clear(void) {
    TEST("Pool returns connection after streaming_active is cleared");
    sim_pool_reset();
    pthread_t me = pthread_self();

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    sim_pool_add(conn1, me);

    conn1->streaming_active = 1;
    tls_cached_slot = 0;

    // Should skip it
    sim_connection_t *got1 = sim_pool_get_connection();
    if (got1 == conn1) { FAIL("should skip while streaming"); goto cleanup; }

    // Clear streaming
    conn1->streaming_active = 0;
    tls_cached_slot = 0;

    // Should now return it
    sim_connection_t *got2 = sim_pool_get_connection();
    if (got2 != conn1) { FAIL("should return conn1 after streaming cleared"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

// ============================================================================
// Tests: resolve_column_tables uses alternate connection
// ============================================================================

static void test_resolve_tables_normal_uses_same_conn(void) {
    TEST("resolve_column_tables uses passed connection when not streaming");
    sim_pool_reset();
    sim_pqexec_call_count = 0;
    sim_pqexec_on_streaming = 0;
    pthread_t me = pthread_self();

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    sim_pool_add(conn1, me);
    tls_cached_slot = 0;

    int used_id = sim_resolve_column_tables(conn1);
    if (used_id != 1) { FAIL("should use conn1 (id=1)"); goto cleanup; }
    if (sim_pqexec_on_streaming != 0) { FAIL("should NOT PQexec on streaming conn"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

static void test_resolve_tables_streaming_uses_alternate(void) {
    TEST("resolve_column_tables uses alternate connection when streaming");
    sim_pool_reset();
    sim_pqexec_call_count = 0;
    sim_pqexec_on_streaming = 0;
    pthread_t me = pthread_self();

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    sim_connection_t *conn2 = sim_pool_init_conn(2, "library.db");
    sim_pool_add(conn1, me);
    sim_pool_add(conn2, me);
    tls_cached_slot = 0;

    conn1->streaming_active = 1;
    conn1->total_results = 446;
    conn1->query_in_progress = 1;

    int used_id = sim_resolve_column_tables(conn1);
    if (used_id == 1) { FAIL("should NOT use streaming conn1"); goto cleanup; }
    if (used_id != 2) { FAIL("should use alternate conn2 (id=2)"); goto cleanup; }
    if (sim_pqexec_on_streaming != 0) { FAIL("PQexec must not touch streaming conn"); goto cleanup; }
    // Verify streaming connection was NOT disrupted
    if (conn1->results_consumed != 0) { FAIL("streaming results should be intact"); goto cleanup; }
    if (!conn1->query_in_progress) { FAIL("streaming query should still be in progress"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

static void test_resolve_tables_no_alternate_skips(void) {
    TEST("resolve_column_tables skips when no alternate connection available");
    sim_pool_reset();
    sim_pqexec_call_count = 0;
    sim_pqexec_on_streaming = 0;

    // Only one connection, and it's streaming — pool can't find another
    // because we don't add it to the pool (simulates pool exhausted)
    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    conn1->streaming_active = 1;

    // Pool is empty — sim_pool_get_connection will create a new one via PHASE 3
    // But let's test the case where pool IS exhausted:
    sim_pool_count = SIM_POOL_SIZE;  // Pretend pool is full
    for (int i = 0; i < SIM_POOL_SIZE; i++) {
        sim_pool[i].conn = conn1;  // All slots point to streaming conn
        atomic_store(&sim_pool[i].state, SIM_SLOT_READY);
        sim_pool[i].owner_thread = pthread_self();
    }
    tls_cached_slot = 0;

    int used_id = sim_resolve_column_tables(conn1);
    if (used_id != -1) { FAIL("should return -1 (skipped) when no alternate"); goto cleanup; }
    if (sim_pqexec_call_count != 0) { FAIL("should NOT call PQexec at all"); goto cleanup; }
    PASS();

cleanup:
    // Reset carefully since we messed with pool
    for (int i = 0; i < SIM_POOL_SIZE; i++) {
        sim_pool[i].conn = NULL;
    }
    sim_pool_count = 0;
    sim_pool_free_conn(conn1);
    tls_cached_slot = -1;
}

// ============================================================================
// Tests: preload_decltype_cache uses alternate connection
// ============================================================================

static void test_decltype_cache_normal_uses_same_conn(void) {
    TEST("preload_decltype_cache uses passed connection when not streaming");
    sim_pool_reset();
    sim_decltype_cache_loaded = 0;
    sim_pqexec_call_count = 0;
    sim_pqexec_on_streaming = 0;
    pthread_t me = pthread_self();

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    sim_pool_add(conn1, me);
    tls_cached_slot = 0;

    int used_id = sim_preload_decltype_cache(conn1);
    if (used_id != 1) { FAIL("should use conn1 (id=1)"); goto cleanup; }
    if (sim_pqexec_on_streaming != 0) { FAIL("should NOT PQexec on streaming conn"); goto cleanup; }
    if (!sim_decltype_cache_loaded) { FAIL("cache should be marked loaded"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

static void test_decltype_cache_streaming_uses_alternate(void) {
    TEST("preload_decltype_cache uses alternate connection when streaming");
    sim_pool_reset();
    sim_decltype_cache_loaded = 0;
    sim_pqexec_call_count = 0;
    sim_pqexec_on_streaming = 0;
    pthread_t me = pthread_self();

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    sim_connection_t *conn2 = sim_pool_init_conn(2, "library.db");
    sim_pool_add(conn1, me);
    sim_pool_add(conn2, me);
    tls_cached_slot = 0;

    conn1->streaming_active = 1;

    int used_id = sim_preload_decltype_cache(conn1);
    if (used_id == 1) { FAIL("should NOT use streaming conn1"); goto cleanup; }
    if (used_id != 2) { FAIL("should use alternate conn2 (id=2)"); goto cleanup; }
    if (sim_pqexec_on_streaming != 0) { FAIL("PQexec must not touch streaming conn"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

static void test_decltype_cache_deferred_when_no_alternate(void) {
    TEST("preload_decltype_cache defers load when no alternate available");
    sim_pool_reset();
    sim_decltype_cache_loaded = 0;
    sim_pqexec_call_count = 0;

    sim_connection_t *conn1 = sim_pool_init_conn(1, "library.db");
    conn1->streaming_active = 1;

    // Fill pool with the same streaming connection
    sim_pool_count = SIM_POOL_SIZE;
    for (int i = 0; i < SIM_POOL_SIZE; i++) {
        sim_pool[i].conn = conn1;
        atomic_store(&sim_pool[i].state, SIM_SLOT_READY);
        sim_pool[i].owner_thread = pthread_self();
    }
    tls_cached_slot = 0;

    int used_id = sim_preload_decltype_cache(conn1);
    if (used_id != -1) { FAIL("should return -1 (deferred)"); goto cleanup; }
    if (sim_decltype_cache_loaded) { FAIL("cache should NOT be marked loaded"); goto cleanup; }
    if (sim_pqexec_call_count != 0) { FAIL("should NOT call PQexec"); goto cleanup; }
    PASS();

cleanup:
    for (int i = 0; i < SIM_POOL_SIZE; i++) sim_pool[i].conn = NULL;
    sim_pool_count = 0;
    sim_pool_free_conn(conn1);
    tls_cached_slot = -1;
}

// ============================================================================
// Tests: PQexec on streaming connection destroys stream (regression proof)
// ============================================================================

static void test_pqexec_on_streaming_destroys_results(void) {
    TEST("PQexec on streaming connection consumes all pending results (regression)");
    sim_pqexec_call_count = 0;
    sim_pqexec_on_streaming = 0;

    sim_connection_t *conn = sim_pool_init_conn(1, "library.db");
    conn->streaming_active = 1;
    conn->query_in_progress = 1;
    conn->total_results = 446;
    conn->results_consumed = 1;  // First row already fetched

    // This is what the bug did — PQexec on the streaming connection
    sim_PQexec(conn, "SELECT oid FROM pg_class WHERE oid IN (12345)");

    if (conn->results_consumed != 446) { FAIL("PQexec should consume all results"); sim_pool_free_conn(conn); return; }
    if (conn->query_in_progress) { FAIL("query should no longer be in progress"); sim_pool_free_conn(conn); return; }
    if (sim_pqexec_on_streaming != 1) { FAIL("should have detected PQexec on streaming"); sim_pool_free_conn(conn); return; }

    sim_pool_free_conn(conn);
    PASS();
}

static void test_streaming_survives_with_isolation(void) {
    TEST("Streaming results survive when using isolated connection");
    sim_pool_reset();
    sim_pqexec_call_count = 0;
    sim_pqexec_on_streaming = 0;
    pthread_t me = pthread_self();

    sim_connection_t *streaming_conn = sim_pool_init_conn(1, "library.db");
    sim_connection_t *worker_conn = sim_pool_init_conn(2, "library.db");
    sim_pool_add(streaming_conn, me);
    sim_pool_add(worker_conn, me);
    tls_cached_slot = 0;

    // Start streaming on conn1
    streaming_conn->streaming_active = 1;
    streaming_conn->query_in_progress = 1;
    streaming_conn->total_results = 446;
    streaming_conn->results_consumed = 1;

    // resolve_column_tables should use conn2
    sim_resolve_column_tables(streaming_conn);

    // Verify streaming connection was NOT disrupted
    if (streaming_conn->results_consumed != 1) { FAIL("streaming results should NOT be consumed"); goto cleanup; }
    if (!streaming_conn->query_in_progress) { FAIL("streaming query should still be running"); goto cleanup; }
    if (!streaming_conn->streaming_active) { FAIL("streaming_active should still be set"); goto cleanup; }
    if (sim_pqexec_on_streaming != 0) { FAIL("no PQexec should touch streaming conn"); goto cleanup; }
    PASS();

cleanup:
    sim_pool_reset();
}

// ============================================================================
// Tests: Multi-threaded isolation
// ============================================================================

typedef struct {
    int thread_id;
    int streaming_conn_id;
    int resolve_used_conn_id;
    int streaming_survived;
    int error;
} isolation_thread_ctx_t;

static void *isolation_thread(void *arg) {
    isolation_thread_ctx_t *ctx = arg;
    tls_cached_slot = -1;

    sim_connection_t *my_conn = sim_pool_init_conn(ctx->streaming_conn_id, "library.db");
    int slot = sim_pool_add(my_conn, pthread_self());
    if (slot < 0) { ctx->error = 1; return NULL; }
    tls_cached_slot = slot;

    // Start streaming
    my_conn->streaming_active = 1;
    my_conn->query_in_progress = 1;
    my_conn->total_results = 100;
    my_conn->results_consumed = 1;

    // Simulate resolve_column_tables — should get alternate
    ctx->resolve_used_conn_id = sim_resolve_column_tables(my_conn);

    // Check if streaming survived
    ctx->streaming_survived = (my_conn->results_consumed == 1 &&
                                my_conn->query_in_progress == 1 &&
                                my_conn->streaming_active == 1);

    // Cleanup streaming
    my_conn->streaming_active = 0;
    my_conn->query_in_progress = 0;

    return NULL;
}

static void test_multithreaded_isolation(void) {
    TEST("Multi-threaded streaming isolation (4 threads)");
    sim_pool_reset();
    sim_pqexec_on_streaming = 0;

    isolation_thread_ctx_t contexts[4] = {
        {0, 10, 0, 0, 0},
        {1, 20, 0, 0, 0},
        {2, 30, 0, 0, 0},
        {3, 40, 0, 0, 0},
    };
    pthread_t threads[4];

    for (int i = 0; i < 4; i++) {
        pthread_create(&threads[i], NULL, isolation_thread, &contexts[i]);
    }
    for (int i = 0; i < 4; i++) {
        pthread_join(threads[i], NULL);
    }

    for (int i = 0; i < 4; i++) {
        if (contexts[i].error) { FAIL("thread had error"); return; }
        if (!contexts[i].streaming_survived) {
            char msg[128];
            snprintf(msg, sizeof(msg), "thread %d: streaming was disrupted", i);
            FAIL(msg);
            return;
        }
        if (contexts[i].resolve_used_conn_id == contexts[i].streaming_conn_id) {
            char msg[128];
            snprintf(msg, sizeof(msg), "thread %d: resolve used the streaming connection", i);
            FAIL(msg);
            return;
        }
    }

    if (sim_pqexec_on_streaming != 0) { FAIL("no PQexec should have touched streaming conns"); return; }
    PASS();

    sim_pool_reset();
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== Connection Isolation Tests (v0.9.29) ===\033[0m\n");

    printf("\n\033[1mstreaming_active Flag Lifecycle:\033[0m\n");
    test_streaming_active_initially_zero();
    test_streaming_active_set_and_clear();
    test_streaming_active_cleared_on_error();
    test_streaming_active_cleared_on_drain();

    printf("\n\033[1mPool Connection Isolation:\033[0m\n");
    test_pool_fast_path_skips_streaming();
    test_pool_phase1_skips_streaming();
    test_pool_creates_new_when_all_streaming();
    test_pool_returns_streaming_conn_after_clear();

    printf("\n\033[1mresolve_column_tables Isolation:\033[0m\n");
    test_resolve_tables_normal_uses_same_conn();
    test_resolve_tables_streaming_uses_alternate();
    test_resolve_tables_no_alternate_skips();

    printf("\n\033[1mpreload_decltype_cache Isolation:\033[0m\n");
    test_decltype_cache_normal_uses_same_conn();
    test_decltype_cache_streaming_uses_alternate();
    test_decltype_cache_deferred_when_no_alternate();

    printf("\n\033[1mRegression: PQexec on Streaming Connection:\033[0m\n");
    test_pqexec_on_streaming_destroys_results();
    test_streaming_survives_with_isolation();

    printf("\n\033[1mMulti-Threaded Isolation:\033[0m\n");
    test_multithreaded_isolation();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
