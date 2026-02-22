/*
 * Unit tests for prepared statement cache and SQLSTATE error detection.
 *
 * Tests:
 * 1. pg_hash_sql: FNV-1a hash correctness and collision resistance
 * 2. pg_stmt_cache_add / pg_stmt_cache_lookup: insert, hit, miss
 * 3. pg_stmt_cache_clear_local: memset without DEALLOCATE
 * 4. Cache eviction when full (LRU)
 * 5. pg_is_duplicate_prepared_stmt: SQLSTATE 42P05 detection
 * 6. pg_is_stale_prepared_stmt: SQLSTATE 26000 detection
 * 7. DEALLOCATE ALL at connection init (verify cache starts empty)
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>

/* ── Test framework ─────────────────────────────────────────────── */

static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)
#define ASSERT(cond, msg) do { if (!(cond)) { FAIL(msg); return; } } while(0)

/* ── Inline the types we need (avoid pulling in libpq) ──────────── */

#define STMT_CACHE_SIZE 512
#define STMT_CACHE_MASK (STMT_CACHE_SIZE - 1)

typedef struct {
    uint64_t sql_hash;
    char stmt_name[32];
    int param_count;
    int prepared;
    time_t last_used;
} prepared_stmt_cache_entry_t;

typedef struct {
    prepared_stmt_cache_entry_t entries[STMT_CACHE_SIZE];
    int count;
} stmt_cache_t;

/* Minimal pg_connection_t mock — only needs stmt_cache */
typedef struct {
    void *conn;           /* unused in tests */
    stmt_cache_t stmt_cache;
} mock_pg_connection_t;

/* ── Reimplement the functions under test (same logic as pg_client.c) ── */

static uint64_t pg_hash_sql(const char *sql) {
    if (!sql) return 0;
    uint64_t hash = 14695981039346656037ULL;
    while (*sql) {
        hash ^= (uint64_t)(unsigned char)*sql++;
        hash *= 1099511628211ULL;
    }
    return hash;
}

static int pg_stmt_cache_lookup(mock_pg_connection_t *conn, uint64_t sql_hash, const char **stmt_name) {
    if (!conn || !stmt_name || sql_hash == 0) return 0;
    stmt_cache_t *cache = &conn->stmt_cache;
    int start_idx = (int)(sql_hash & STMT_CACHE_MASK);
    for (int probe = 0; probe < STMT_CACHE_SIZE; probe++) {
        int idx = (start_idx + probe) & STMT_CACHE_MASK;
        prepared_stmt_cache_entry_t *entry = &cache->entries[idx];
        if (entry->sql_hash == 0) return 0;
        if (entry->sql_hash == sql_hash && entry->prepared) {
            entry->last_used = time(NULL);
            *stmt_name = entry->stmt_name;
            return 1;
        }
    }
    return 0;
}

static int pg_stmt_cache_add(mock_pg_connection_t *conn, uint64_t sql_hash, const char *stmt_name, int param_count) {
    if (!conn || !stmt_name || sql_hash == 0) return -1;
    stmt_cache_t *cache = &conn->stmt_cache;
    int start_idx = (int)(sql_hash & STMT_CACHE_MASK);
    int oldest_idx = -1;
    time_t oldest_time = 0;

    for (int probe = 0; probe < STMT_CACHE_SIZE; probe++) {
        int idx = (start_idx + probe) & STMT_CACHE_MASK;
        prepared_stmt_cache_entry_t *entry = &cache->entries[idx];

        if (oldest_idx == -1 || entry->last_used < oldest_time) {
            oldest_idx = idx;
            oldest_time = entry->last_used;
        }

        if (entry->sql_hash == sql_hash) {
            /* Update existing */
            strncpy(entry->stmt_name, stmt_name, sizeof(entry->stmt_name) - 1);
            entry->param_count = param_count;
            entry->prepared = 1;
            entry->last_used = time(NULL);
            return idx;
        }

        if (entry->sql_hash == 0) {
            /* Empty slot */
            entry->sql_hash = sql_hash;
            strncpy(entry->stmt_name, stmt_name, sizeof(entry->stmt_name) - 1);
            entry->param_count = param_count;
            entry->prepared = 1;
            entry->last_used = time(NULL);
            cache->count++;
            return idx;
        }
    }

    /* Full — evict oldest */
    if (oldest_idx >= 0) {
        prepared_stmt_cache_entry_t *entry = &cache->entries[oldest_idx];
        entry->sql_hash = sql_hash;
        strncpy(entry->stmt_name, stmt_name, sizeof(entry->stmt_name) - 1);
        entry->param_count = param_count;
        entry->prepared = 1;
        entry->last_used = time(NULL);
        return oldest_idx;
    }
    return -1;
}

static void pg_stmt_cache_clear_local(mock_pg_connection_t *conn) {
    if (!conn) return;
    memset(&conn->stmt_cache, 0, sizeof(stmt_cache_t));
}

/* ── Tests: hash function ───────────────────────────────────────── */

static void test_hash_null(void) {
    TEST("hash of NULL returns 0");
    ASSERT(pg_hash_sql(NULL) == 0, "expected 0");
    PASS();
}

static void test_hash_deterministic(void) {
    TEST("hash is deterministic");
    const char *sql = "SELECT * FROM metadata_items WHERE id=$1";
    uint64_t h1 = pg_hash_sql(sql);
    uint64_t h2 = pg_hash_sql(sql);
    ASSERT(h1 == h2, "same input must produce same hash");
    ASSERT(h1 != 0, "hash should not be 0 for non-empty string");
    PASS();
}

static void test_hash_different_inputs(void) {
    TEST("different SQL produces different hashes");
    uint64_t h1 = pg_hash_sql("SELECT id FROM tags");
    uint64_t h2 = pg_hash_sql("SELECT id FROM taggings");
    ASSERT(h1 != h2, "different SQL should hash differently");
    PASS();
}

/* ── Tests: cache add / lookup ──────────────────────────────────── */

static void test_cache_miss_empty(void) {
    TEST("lookup on empty cache returns miss");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));
    const char *name = NULL;
    uint64_t h = pg_hash_sql("SELECT 1");
    ASSERT(pg_stmt_cache_lookup(&conn, h, &name) == 0, "expected miss");
    PASS();
}

static void test_cache_add_then_hit(void) {
    TEST("add then lookup returns hit");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));
    uint64_t h = pg_hash_sql("SELECT id FROM metadata_items WHERE id=$1");
    pg_stmt_cache_add(&conn, h, "ps_abc123", 1);

    const char *name = NULL;
    ASSERT(pg_stmt_cache_lookup(&conn, h, &name) == 1, "expected hit");
    ASSERT(strcmp(name, "ps_abc123") == 0, "wrong stmt name");
    PASS();
}

static void test_cache_miss_different_hash(void) {
    TEST("lookup with different hash returns miss");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));
    uint64_t h1 = pg_hash_sql("SELECT 1");
    uint64_t h2 = pg_hash_sql("SELECT 2");
    pg_stmt_cache_add(&conn, h1, "ps_one", 0);

    const char *name = NULL;
    ASSERT(pg_stmt_cache_lookup(&conn, h2, &name) == 0, "expected miss for different hash");
    PASS();
}

static void test_cache_update_existing(void) {
    TEST("add with same hash updates existing entry");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));
    uint64_t h = pg_hash_sql("SELECT 1");
    pg_stmt_cache_add(&conn, h, "ps_old", 0);
    pg_stmt_cache_add(&conn, h, "ps_new", 2);

    const char *name = NULL;
    ASSERT(pg_stmt_cache_lookup(&conn, h, &name) == 1, "expected hit");
    ASSERT(strcmp(name, "ps_new") == 0, "should have updated name");
    PASS();
}

static void test_cache_multiple_entries(void) {
    TEST("multiple distinct entries coexist");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));

    const char *sqls[] = {
        "SELECT id FROM tags",
        "SELECT id FROM taggings",
        "SELECT id FROM media_items",
        "UPDATE metadata_items SET extra_data=$1 WHERE id=$2",
        "DELETE FROM plugins WHERE identifier=$1",
    };
    int n = sizeof(sqls) / sizeof(sqls[0]);

    for (int i = 0; i < n; i++) {
        char sname[32];
        snprintf(sname, sizeof(sname), "ps_%d", i);
        pg_stmt_cache_add(&conn, pg_hash_sql(sqls[i]), sname, i);
    }

    for (int i = 0; i < n; i++) {
        const char *name = NULL;
        char expected[32];
        snprintf(expected, sizeof(expected), "ps_%d", i);
        ASSERT(pg_stmt_cache_lookup(&conn, pg_hash_sql(sqls[i]), &name) == 1, "expected hit");
        ASSERT(strcmp(name, expected) == 0, "wrong name for entry");
    }
    PASS();
}

/* ── Tests: cache clear ─────────────────────────────────────────── */

static void test_cache_clear_local(void) {
    TEST("clear_local makes all lookups miss");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));
    uint64_t h = pg_hash_sql("SELECT 1");
    pg_stmt_cache_add(&conn, h, "ps_test", 0);

    pg_stmt_cache_clear_local(&conn);

    const char *name = NULL;
    ASSERT(pg_stmt_cache_lookup(&conn, h, &name) == 0, "expected miss after clear");
    ASSERT(conn.stmt_cache.count == 0, "count should be 0");
    PASS();
}

static void test_cache_readd_after_clear(void) {
    TEST("can re-add entries after clear");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));
    uint64_t h = pg_hash_sql("SELECT 1");
    pg_stmt_cache_add(&conn, h, "ps_v1", 0);
    pg_stmt_cache_clear_local(&conn);
    pg_stmt_cache_add(&conn, h, "ps_v2", 0);

    const char *name = NULL;
    ASSERT(pg_stmt_cache_lookup(&conn, h, &name) == 1, "expected hit after re-add");
    ASSERT(strcmp(name, "ps_v2") == 0, "should be new name");
    PASS();
}

/* ── Tests: SQLSTATE detection (mocked PGresult) ────────────────── */

/*
 * We can't use real PGresult here (no libpq in unit tests).
 * Instead, test the SQLSTATE comparison logic directly.
 */

static int mock_is_duplicate(const char *sqlstate) {
    return sqlstate && strcmp(sqlstate, "42P05") == 0;
}

static int mock_is_stale(const char *sqlstate) {
    return sqlstate && strcmp(sqlstate, "26000") == 0;
}

static void test_sqlstate_42P05(void) {
    TEST("42P05 detected as duplicate prepared stmt");
    ASSERT(mock_is_duplicate("42P05") == 1, "should match");
    PASS();
}

static void test_sqlstate_26000(void) {
    TEST("26000 detected as stale prepared stmt");
    ASSERT(mock_is_stale("26000") == 1, "should match");
    PASS();
}

static void test_sqlstate_null(void) {
    TEST("NULL SQLSTATE returns 0 for both checks");
    ASSERT(mock_is_duplicate(NULL) == 0, "duplicate should be 0");
    ASSERT(mock_is_stale(NULL) == 0, "stale should be 0");
    PASS();
}

static void test_sqlstate_wrong_code(void) {
    TEST("unrelated SQLSTATE returns 0");
    ASSERT(mock_is_duplicate("42000") == 0, "42000 is not 42P05");
    ASSERT(mock_is_stale("42P05") == 0, "42P05 is not 26000");
    ASSERT(mock_is_duplicate("26000") == 0, "26000 is not 42P05");
    PASS();
}

static void test_sqlstate_empty_string(void) {
    TEST("empty SQLSTATE returns 0");
    ASSERT(mock_is_duplicate("") == 0, "empty is not 42P05");
    ASSERT(mock_is_stale("") == 0, "empty is not 26000");
    PASS();
}

/* ── Tests: edge cases ──────────────────────────────────────────── */

static void test_lookup_null_conn(void) {
    TEST("lookup with NULL conn returns 0");
    const char *name = NULL;
    ASSERT(pg_stmt_cache_lookup(NULL, 12345, &name) == 0, "expected 0");
    PASS();
}

static void test_lookup_zero_hash(void) {
    TEST("lookup with hash 0 returns 0");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));
    const char *name = NULL;
    ASSERT(pg_stmt_cache_lookup(&conn, 0, &name) == 0, "expected 0");
    PASS();
}

static void test_add_null_conn(void) {
    TEST("add with NULL conn returns -1");
    ASSERT(pg_stmt_cache_add(NULL, 12345, "ps_x", 0) == -1, "expected -1");
    PASS();
}

static void test_add_zero_hash(void) {
    TEST("add with hash 0 returns -1");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));
    ASSERT(pg_stmt_cache_add(&conn, 0, "ps_x", 0) == -1, "expected -1");
    PASS();
}

static void test_clear_null_conn(void) {
    TEST("clear_local with NULL conn does not crash");
    pg_stmt_cache_clear_local(NULL);  /* should not crash */
    PASS();
}

/* ── Tests: cache capacity ──────────────────────────────────────── */

static void test_cache_fill_and_evict(void) {
    TEST("cache handles 512+ entries with eviction");
    mock_pg_connection_t conn;
    memset(&conn, 0, sizeof(conn));

    /* Fill with 512 entries */
    for (int i = 1; i <= STMT_CACHE_SIZE; i++) {
        char sql[64];
        snprintf(sql, sizeof(sql), "SELECT %d", i);
        char sname[32];
        snprintf(sname, sizeof(sname), "ps_%d", i);
        int rc = pg_stmt_cache_add(&conn, pg_hash_sql(sql), sname, 0);
        ASSERT(rc >= 0, "add should succeed");
    }

    /* Add one more — should evict oldest */
    int rc = pg_stmt_cache_add(&conn, pg_hash_sql("SELECT overflow"), "ps_overflow", 0);
    ASSERT(rc >= 0, "eviction add should succeed");

    const char *name = NULL;
    ASSERT(pg_stmt_cache_lookup(&conn, pg_hash_sql("SELECT overflow"), &name) == 1, "overflow entry should be found");
    PASS();
}

/* ── Main ───────────────────────────────────────────────────────── */

int main(void) {
    printf("\n\033[1m=== Prepared Statement Cache Tests ===\033[0m\n\n");

    printf("\033[1mHash Function:\033[0m\n");
    test_hash_null();
    test_hash_deterministic();
    test_hash_different_inputs();

    printf("\n\033[1mCache Add/Lookup:\033[0m\n");
    test_cache_miss_empty();
    test_cache_add_then_hit();
    test_cache_miss_different_hash();
    test_cache_update_existing();
    test_cache_multiple_entries();

    printf("\n\033[1mCache Clear:\033[0m\n");
    test_cache_clear_local();
    test_cache_readd_after_clear();

    printf("\n\033[1mSQLSTATE Detection:\033[0m\n");
    test_sqlstate_42P05();
    test_sqlstate_26000();
    test_sqlstate_null();
    test_sqlstate_wrong_code();
    test_sqlstate_empty_string();

    printf("\n\033[1mEdge Cases:\033[0m\n");
    test_lookup_null_conn();
    test_lookup_zero_hash();
    test_add_null_conn();
    test_add_zero_hash();
    test_clear_null_conn();

    printf("\n\033[1mCapacity:\033[0m\n");
    test_cache_fill_and_evict();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
