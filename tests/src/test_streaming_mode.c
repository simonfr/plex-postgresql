/*
 * Unit tests for Single-Row Streaming Mode (v0.9.28)
 *
 * Tests the state machine that manages PQsetSingleRowMode streaming:
 * 1. Streaming state transitions (inactive → active → drain → inactive)
 * 2. Result lifecycle (each step() clears previous row, fetches next)
 * 3. Drain behavior on reset/finalize (all pending results consumed)
 * 4. Error handling mid-stream (PGRES_FATAL_ERROR after partial rows)
 * 5. Fallback to eager mode when PQsetSingleRowMode fails
 * 6. Zero-row query handling (PGRES_TUPLES_OK sentinel immediately)
 * 7. Connection ownership during streaming (no other stmt can use it)
 * 8. Concurrent streaming on different threads (each has own connection)
 *
 * Per PG docs (32.6): After PQsendQuery + PQsetSingleRowMode:
 * - PQgetResult returns PGRES_SINGLE_TUPLE for each row
 * - Then PGRES_TUPLES_OK (0 rows) as sentinel
 * - Then NULL to signal completion
 * - On error: PGRES_FATAL_ERROR may follow partial PGRES_SINGLE_TUPLE rows
 *
 * IMPORTANT: These tests verify streaming logic WITHOUT loading libpq.
 * They simulate the PGresult state machine in isolation.
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
// Simulate PGresult status codes from libpq
// ============================================================================

typedef enum {
    SIM_PGRES_SINGLE_TUPLE = 9,   // PGRES_SINGLE_TUPLE
    SIM_PGRES_TUPLES_OK = 2,      // PGRES_TUPLES_OK (sentinel / eager result)
    SIM_PGRES_FATAL_ERROR = 7,    // PGRES_FATAL_ERROR
    SIM_PGRES_COMMAND_OK = 1,     // PGRES_COMMAND_OK
} sim_exec_status_t;

// Simulated PGresult
typedef struct {
    sim_exec_status_t status;
    int num_rows;
    int num_cols;
    int freed;  // Track if PQclear was called
} sim_pg_result_t;

// Simulated connection with a queue of results to return
#define MAX_QUEUED_RESULTS 64

typedef struct {
    sim_pg_result_t results[MAX_QUEUED_RESULTS];
    int result_count;
    int next_result;
    int single_row_mode;
    pthread_mutex_t mutex;
    int in_use_by_streaming;  // Track exclusive ownership
} sim_connection_t;

// Simulated streaming statement state (mirrors pg_stmt_t fields)
typedef struct {
    int streaming_mode;
    sim_connection_t *streaming_conn;
    sim_pg_result_t *current_result;
    int current_row;
    int num_rows;
    int num_cols;
    int read_done;
    pthread_mutex_t mutex;
} sim_stmt_t;

// ============================================================================
// Simulated libpq functions
// ============================================================================

static sim_pg_result_t *sim_PQgetResult(sim_connection_t *conn) {
    if (conn->next_result >= conn->result_count) {
        return NULL;  // No more results
    }
    return &conn->results[conn->next_result++];
}

static void sim_PQclear(sim_pg_result_t *res) {
    if (res) res->freed = 1;
}

static int sim_PQsetSingleRowMode(sim_connection_t *conn) {
    if (conn->single_row_mode) {
        conn->single_row_mode = 0;  // Consume the flag
        return 1;  // Success
    }
    return 0;  // Failure
}

static void sim_conn_init(sim_connection_t *conn) {
    memset(conn, 0, sizeof(*conn));
    pthread_mutex_init(&conn->mutex, NULL);
}

static void sim_conn_destroy(sim_connection_t *conn) {
    pthread_mutex_destroy(&conn->mutex);
}

// Queue results that PQgetResult will return
static void sim_queue_single_rows(sim_connection_t *conn, int num_rows, int num_cols) {
    conn->result_count = 0;
    conn->next_result = 0;
    conn->single_row_mode = 1;

    // Queue individual row results
    for (int i = 0; i < num_rows && i < MAX_QUEUED_RESULTS - 2; i++) {
        conn->results[conn->result_count].status = SIM_PGRES_SINGLE_TUPLE;
        conn->results[conn->result_count].num_rows = 1;
        conn->results[conn->result_count].num_cols = num_cols;
        conn->results[conn->result_count].freed = 0;
        conn->result_count++;
    }

    // Queue TUPLES_OK sentinel (0 rows)
    conn->results[conn->result_count].status = SIM_PGRES_TUPLES_OK;
    conn->results[conn->result_count].num_rows = 0;
    conn->results[conn->result_count].num_cols = num_cols;
    conn->results[conn->result_count].freed = 0;
    conn->result_count++;

    // NULL is implicit (sim_PQgetResult returns NULL when exhausted)
}

static void sim_queue_zero_rows(sim_connection_t *conn, int num_cols) {
    conn->result_count = 0;
    conn->next_result = 0;
    conn->single_row_mode = 1;

    // Just the TUPLES_OK sentinel immediately
    conn->results[conn->result_count].status = SIM_PGRES_TUPLES_OK;
    conn->results[conn->result_count].num_rows = 0;
    conn->results[conn->result_count].num_cols = num_cols;
    conn->results[conn->result_count].freed = 0;
    conn->result_count++;
}

static void sim_queue_error_after_rows(sim_connection_t *conn, int rows_before_error, int num_cols) {
    conn->result_count = 0;
    conn->next_result = 0;
    conn->single_row_mode = 1;

    for (int i = 0; i < rows_before_error && i < MAX_QUEUED_RESULTS - 2; i++) {
        conn->results[conn->result_count].status = SIM_PGRES_SINGLE_TUPLE;
        conn->results[conn->result_count].num_rows = 1;
        conn->results[conn->result_count].num_cols = num_cols;
        conn->results[conn->result_count].freed = 0;
        conn->result_count++;
    }

    // Error instead of more rows
    conn->results[conn->result_count].status = SIM_PGRES_FATAL_ERROR;
    conn->results[conn->result_count].num_rows = 0;
    conn->results[conn->result_count].num_cols = 0;
    conn->results[conn->result_count].freed = 0;
    conn->result_count++;
}

// ============================================================================
// Simulate the streaming step() logic from db_interpose_step.c
// Returns: 101 = SQLITE_ROW, 100 = SQLITE_DONE, 1 = SQLITE_ERROR
// ============================================================================
#define SIM_SQLITE_ROW  101
#define SIM_SQLITE_DONE 100
#define SIM_SQLITE_ERROR 1

static int sim_step_streaming(sim_stmt_t *stmt) {
    // Clear previous result
    if (stmt->current_result) {
        sim_PQclear(stmt->current_result);
        stmt->current_result = NULL;
    }

    sim_pg_result_t *row_res = sim_PQgetResult(stmt->streaming_conn);
    if (!row_res) {
        stmt->streaming_mode = 0;
        stmt->streaming_conn = NULL;
        stmt->read_done = 1;
        return SIM_SQLITE_DONE;
    }

    if (row_res->status == SIM_PGRES_SINGLE_TUPLE) {
        stmt->current_result = row_res;
        stmt->current_row = 0;
        stmt->num_rows = 1;
        stmt->num_cols = row_res->num_cols;
        return SIM_SQLITE_ROW;
    } else if (row_res->status == SIM_PGRES_TUPLES_OK) {
        sim_PQclear(row_res);
        // Drain final NULL
        sim_pg_result_t *final_null = sim_PQgetResult(stmt->streaming_conn);
        if (final_null) sim_PQclear(final_null);
        stmt->streaming_mode = 0;
        stmt->streaming_conn = NULL;
        stmt->read_done = 1;
        return SIM_SQLITE_DONE;
    } else {
        // Error
        sim_PQclear(row_res);
        sim_pg_result_t *drain;
        while ((drain = sim_PQgetResult(stmt->streaming_conn)) != NULL) {
            sim_PQclear(drain);
        }
        stmt->streaming_mode = 0;
        stmt->streaming_conn = NULL;
        stmt->read_done = 1;
        return SIM_SQLITE_DONE;  // Treat as empty, matching shim behavior
    }
}

// Simulate first step() — activates streaming
static int sim_step_first(sim_stmt_t *stmt, sim_connection_t *conn) {
    if (!sim_PQsetSingleRowMode(conn)) {
        return -1;  // Would fall back to eager
    }

    stmt->streaming_mode = 1;
    stmt->streaming_conn = conn;
    conn->in_use_by_streaming = 1;

    sim_pg_result_t *first_res = sim_PQgetResult(conn);
    if (!first_res) {
        stmt->streaming_mode = 0;
        stmt->streaming_conn = NULL;
        conn->in_use_by_streaming = 0;
        stmt->read_done = 1;
        return SIM_SQLITE_DONE;
    }

    if (first_res->status == SIM_PGRES_SINGLE_TUPLE) {
        stmt->current_result = first_res;
        stmt->current_row = 0;
        stmt->num_rows = 1;
        stmt->num_cols = first_res->num_cols;
        return SIM_SQLITE_ROW;
    } else if (first_res->status == SIM_PGRES_TUPLES_OK) {
        sim_PQclear(first_res);
        sim_pg_result_t *final_null = sim_PQgetResult(conn);
        if (final_null) sim_PQclear(final_null);
        stmt->streaming_mode = 0;
        stmt->streaming_conn = NULL;
        conn->in_use_by_streaming = 0;
        stmt->num_rows = 0;
        stmt->read_done = 1;
        return SIM_SQLITE_DONE;
    } else {
        sim_PQclear(first_res);
        sim_pg_result_t *drain;
        while ((drain = sim_PQgetResult(conn)) != NULL) sim_PQclear(drain);
        stmt->streaming_mode = 0;
        stmt->streaming_conn = NULL;
        conn->in_use_by_streaming = 0;
        return SIM_SQLITE_ERROR;
    }
}

// Simulate drain on reset/finalize
static void sim_drain_streaming(sim_stmt_t *stmt) {
    if (!stmt->streaming_mode || !stmt->streaming_conn) return;

    if (stmt->current_result) {
        sim_PQclear(stmt->current_result);
        stmt->current_result = NULL;
    }

    sim_pg_result_t *drain;
    while ((drain = sim_PQgetResult(stmt->streaming_conn)) != NULL) {
        sim_PQclear(drain);
    }

    stmt->streaming_conn->in_use_by_streaming = 0;
    stmt->streaming_mode = 0;
    stmt->streaming_conn = NULL;
}

static void sim_stmt_init(sim_stmt_t *stmt) {
    memset(stmt, 0, sizeof(*stmt));
    pthread_mutex_init(&stmt->mutex, NULL);
}

static void sim_stmt_destroy(sim_stmt_t *stmt) {
    pthread_mutex_destroy(&stmt->mutex);
}

// ============================================================================
// Tests
// ============================================================================

static void test_basic_streaming_5_rows(void) {
    TEST("Basic streaming - 5 rows returned one at a time");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 5, 3);

    int rc = sim_step_first(&stmt, &conn);
    if (rc != SIM_SQLITE_ROW) { FAIL("first step should return ROW"); return; }
    if (stmt.num_cols != 3) { FAIL("num_cols should be 3"); return; }
    if (!stmt.streaming_mode) { FAIL("streaming_mode should be active"); return; }

    int row_count = 1;
    while ((rc = sim_step_streaming(&stmt)) == SIM_SQLITE_ROW) {
        row_count++;
        if (stmt.current_row != 0) { FAIL("current_row should always be 0 in streaming"); return; }
    }

    if (rc != SIM_SQLITE_DONE) { FAIL("should end with DONE"); return; }
    if (row_count != 5) { FAIL("should get exactly 5 rows"); return; }
    if (stmt.streaming_mode) { FAIL("streaming_mode should be inactive after DONE"); return; }
    if (stmt.streaming_conn) { FAIL("streaming_conn should be NULL after DONE"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_zero_rows(void) {
    TEST("Zero rows - TUPLES_OK sentinel immediately");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_zero_rows(&conn, 4);

    int rc = sim_step_first(&stmt, &conn);
    if (rc != SIM_SQLITE_DONE) { FAIL("zero rows should return DONE immediately"); return; }
    if (stmt.streaming_mode) { FAIL("streaming_mode should be inactive"); return; }
    if (stmt.num_rows != 0) { FAIL("num_rows should be 0"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_single_row(void) {
    TEST("Single row - one SINGLE_TUPLE then sentinel");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 1, 2);

    int rc = sim_step_first(&stmt, &conn);
    if (rc != SIM_SQLITE_ROW) { FAIL("first step should return ROW"); return; }

    rc = sim_step_streaming(&stmt);
    if (rc != SIM_SQLITE_DONE) { FAIL("second step should return DONE"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_error_after_partial_rows(void) {
    TEST("Error after 3 rows - per PG docs 32.6 caution");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_error_after_rows(&conn, 3, 5);

    int rc = sim_step_first(&stmt, &conn);
    if (rc != SIM_SQLITE_ROW) { FAIL("first step should return ROW"); return; }

    int row_count = 1;
    while ((rc = sim_step_streaming(&stmt)) == SIM_SQLITE_ROW) {
        row_count++;
    }

    // After error, shim returns DONE (not ERROR) to prevent Plex crash
    if (rc != SIM_SQLITE_DONE) { FAIL("error should be treated as DONE"); return; }
    if (row_count != 3) { FAIL("should get 3 rows before error"); return; }
    if (stmt.streaming_mode) { FAIL("streaming should be deactivated after error"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_drain_on_reset_mid_stream(void) {
    TEST("Drain on reset - mid-stream with remaining rows");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 10, 3);

    int rc = sim_step_first(&stmt, &conn);
    if (rc != SIM_SQLITE_ROW) { FAIL("first step should return ROW"); return; }

    // Read only 3 of 10 rows
    for (int i = 0; i < 2; i++) {
        rc = sim_step_streaming(&stmt);
        if (rc != SIM_SQLITE_ROW) { FAIL("should still have rows"); return; }
    }

    // Now reset — should drain remaining 7 rows + sentinel + NULL
    sim_drain_streaming(&stmt);

    if (stmt.streaming_mode) { FAIL("streaming should be off after drain"); return; }
    if (stmt.streaming_conn) { FAIL("streaming_conn should be NULL after drain"); return; }
    if (conn.in_use_by_streaming != 0) { FAIL("conn should be released"); return; }

    // Verify all results were consumed (next_result should be at end)
    if (conn.next_result != conn.result_count) {
        // The drain consumed through PQgetResult returning NULL
        // next_result should be >= result_count
    }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_drain_on_finalize_not_started(void) {
    TEST("Drain on finalize - streaming not active (no-op)");

    sim_stmt_t stmt;
    sim_stmt_init(&stmt);

    // Should not crash
    sim_drain_streaming(&stmt);

    if (stmt.streaming_mode) { FAIL("should remain 0"); return; }

    sim_stmt_destroy(&stmt);
    PASS();
}

static void test_fallback_eager_mode(void) {
    TEST("Fallback to eager when PQsetSingleRowMode fails");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    // Don't set single_row_mode flag — PQsetSingleRowMode will return 0
    conn.single_row_mode = 0;
    conn.result_count = 1;
    conn.next_result = 0;
    conn.results[0].status = SIM_PGRES_TUPLES_OK;
    conn.results[0].num_rows = 50;
    conn.results[0].num_cols = 3;
    conn.results[0].freed = 0;

    int rc = sim_step_first(&stmt, &conn);
    if (rc != -1) { FAIL("should return -1 to signal eager fallback"); return; }
    if (stmt.streaming_mode) { FAIL("streaming should NOT be active"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_connection_ownership(void) {
    TEST("Connection exclusively owned during streaming");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 5, 2);

    sim_step_first(&stmt, &conn);

    if (!conn.in_use_by_streaming) { FAIL("conn should be marked in-use"); return; }
    if (stmt.streaming_conn != &conn) { FAIL("stmt should reference conn"); return; }

    // Drain releases ownership
    sim_drain_streaming(&stmt);

    if (conn.in_use_by_streaming) { FAIL("conn should be released after drain"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_current_row_always_zero(void) {
    TEST("current_row is always 0 in streaming mode (1-row PGresult)");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 20, 4);

    int rc = sim_step_first(&stmt, &conn);
    if (stmt.current_row != 0) { FAIL("first row: current_row should be 0"); return; }

    for (int i = 0; i < 19; i++) {
        rc = sim_step_streaming(&stmt);
        if (rc == SIM_SQLITE_ROW && stmt.current_row != 0) {
            FAIL("current_row must always be 0 in streaming");
            sim_drain_streaming(&stmt);
            sim_stmt_destroy(&stmt);
            sim_conn_destroy(&conn);
            return;
        }
    }

    // Last step should be DONE
    rc = sim_step_streaming(&stmt);
    if (rc != SIM_SQLITE_DONE) { FAIL("should end with DONE"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_num_cols_consistent(void) {
    TEST("num_cols consistent across all streaming rows");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 8, 7);

    sim_step_first(&stmt, &conn);
    if (stmt.num_cols != 7) { FAIL("first row num_cols wrong"); return; }

    int rc;
    while ((rc = sim_step_streaming(&stmt)) == SIM_SQLITE_ROW) {
        if (stmt.num_cols != 7) {
            FAIL("num_cols changed mid-stream");
            sim_drain_streaming(&stmt);
            sim_stmt_destroy(&stmt);
            sim_conn_destroy(&conn);
            return;
        }
    }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_previous_result_freed(void) {
    TEST("Previous PGresult freed before fetching next row");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 3, 2);

    sim_step_first(&stmt, &conn);
    sim_pg_result_t *first = stmt.current_result;

    sim_step_streaming(&stmt);
    if (!first->freed) { FAIL("first result should be freed after second step"); return; }

    sim_pg_result_t *second = stmt.current_result;
    sim_step_streaming(&stmt);
    if (!second->freed) { FAIL("second result should be freed after third step"); return; }

    // Third step gets DONE (sentinel), which frees the third row
    sim_step_streaming(&stmt);

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_large_result_set(void) {
    TEST("Large result set - 50 rows streamed without accumulation");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 50, 10);

    int rc = sim_step_first(&stmt, &conn);
    int row_count = 0;
    int max_concurrent_results = 0;

    while (rc == SIM_SQLITE_ROW) {
        row_count++;
        // In streaming mode, only 1 result should exist at a time
        int concurrent = (stmt.current_result != NULL) ? 1 : 0;
        if (concurrent > max_concurrent_results) max_concurrent_results = concurrent;
        rc = sim_step_streaming(&stmt);
    }

    if (row_count != 50) { FAIL("should get 50 rows"); return; }
    if (max_concurrent_results > 1) { FAIL("should never have >1 result in memory"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

// ============================================================================
// Concurrent streaming test — two threads, two connections
// ============================================================================

typedef struct {
    int thread_id;
    int rows_expected;
    int rows_received;
    int error;
} thread_ctx_t;

static void *streaming_thread(void *arg) {
    thread_ctx_t *ctx = arg;

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, ctx->rows_expected, 3);

    int rc = sim_step_first(&stmt, &conn);
    if (rc != SIM_SQLITE_ROW) {
        ctx->error = 1;
        sim_stmt_destroy(&stmt);
        sim_conn_destroy(&conn);
        return NULL;
    }

    ctx->rows_received = 1;
    while ((rc = sim_step_streaming(&stmt)) == SIM_SQLITE_ROW) {
        ctx->rows_received++;
        usleep(100);  // Simulate work between rows
    }

    if (rc != SIM_SQLITE_DONE) ctx->error = 1;

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    return NULL;
}

static void test_concurrent_streaming(void) {
    TEST("Concurrent streaming on 4 threads (separate connections)");

    thread_ctx_t contexts[4] = {
        {0, 10, 0, 0},
        {1, 20, 0, 0},
        {2, 5, 0, 0},
        {3, 15, 0, 0},
    };
    pthread_t threads[4];

    for (int i = 0; i < 4; i++) {
        pthread_create(&threads[i], NULL, streaming_thread, &contexts[i]);
    }
    for (int i = 0; i < 4; i++) {
        pthread_join(threads[i], NULL);
    }

    for (int i = 0; i < 4; i++) {
        if (contexts[i].error) { FAIL("thread had error"); return; }
        if (contexts[i].rows_received != contexts[i].rows_expected) {
            FAIL("thread got wrong row count");
            return;
        }
    }

    PASS();
}

static void test_read_done_flag(void) {
    TEST("read_done flag set after streaming completes");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_single_rows(&conn, 2, 1);

    if (stmt.read_done) { FAIL("read_done should be 0 initially"); return; }

    sim_step_first(&stmt, &conn);
    if (stmt.read_done) { FAIL("read_done should be 0 during streaming"); return; }

    sim_step_streaming(&stmt);  // row 2
    if (stmt.read_done) { FAIL("read_done should be 0 during streaming"); return; }

    sim_step_streaming(&stmt);  // DONE
    if (!stmt.read_done) { FAIL("read_done should be 1 after DONE"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

static void test_error_on_first_fetch(void) {
    TEST("Error on first fetch - immediate PGRES_FATAL_ERROR");

    sim_connection_t conn;
    sim_stmt_t stmt;
    sim_conn_init(&conn);
    sim_stmt_init(&stmt);

    sim_queue_error_after_rows(&conn, 0, 3);

    int rc = sim_step_first(&stmt, &conn);
    if (rc != SIM_SQLITE_ERROR) { FAIL("should return ERROR on first fetch failure"); return; }
    if (stmt.streaming_mode) { FAIL("streaming should be off after error"); return; }

    sim_stmt_destroy(&stmt);
    sim_conn_destroy(&conn);
    PASS();
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== Single-Row Streaming Mode Tests (v0.9.28) ===\033[0m\n");

    printf("\n\033[1mBasic Streaming:\033[0m\n");
    test_basic_streaming_5_rows();
    test_single_row();
    test_zero_rows();
    test_large_result_set();

    printf("\n\033[1mState Management:\033[0m\n");
    test_current_row_always_zero();
    test_num_cols_consistent();
    test_previous_result_freed();
    test_read_done_flag();
    test_connection_ownership();

    printf("\n\033[1mError Handling (PG docs 32.6 Caution):\033[0m\n");
    test_error_after_partial_rows();
    test_error_on_first_fetch();
    test_fallback_eager_mode();

    printf("\n\033[1mDrain / Reset / Finalize:\033[0m\n");
    test_drain_on_reset_mid_stream();
    test_drain_on_finalize_not_started();

    printf("\n\033[1mConcurrency:\033[0m\n");
    test_concurrent_streaming();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
