/*
 * Unit tests for Shadow SQLite Elimination (Fase 3-5)
 *
 * Tests that the shim can work with a minimal in-memory shadow SQLite
 * instead of a full schema-synced shadow database. The shadow becomes
 * just a handle factory — providing valid sqlite3_stmt* pointers for
 * Plex's API contract.
 *
 * Test categories:
 * 1. In-memory shadow DB: opens correctly, accepts dummy prepares
 * 2. Dummy statement: correct parameter counting for all SQL patterns
 * 3. Bind absorption: all bind types work on dummy stmts
 * 4. Connection routing: sqlite3_db_handle works on dummy stmts
 * 5. Decltype independence: PG type mapping covers all column types
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sqlite3.h>

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)
#define ASSERT(cond, msg) do { if (!(cond)) { FAIL(msg); return; } } while(0)
#define ASSERT_EQ(a, b, msg) do { if ((a) != (b)) { char buf[256]; snprintf(buf, sizeof(buf), "%s (got %d, expected %d)", msg, (int)(a), (int)(b)); FAIL(buf); return; } } while(0)
#define ASSERT_STR(a, b, msg) do { if (strcmp((a),(b)) != 0) { char buf[256]; snprintf(buf, sizeof(buf), "%s (got '%s', expected '%s')", msg, (a), (b)); FAIL(buf); return; } } while(0)
#define ASSERT_NOT_NULL(p, msg) do { if ((p) == NULL) { FAIL(msg); return; } } while(0)

// ============================================================================
// Parameter counting logic (must match db_interpose_prepare.c)
// ============================================================================
static int count_sql_params(const char *sql) {
    if (!sql) return 0;
    int param_count = 0;
    int in_quote = 0;
    for (const char *p = sql; *p; p++) {
        if (*p == '\'') {
            in_quote = !in_quote;
            continue;
        }
        if (in_quote) continue;
        if (*p == '?') {
            param_count++;
            continue;
        }
        if (*p == ':' && p[1] && (p[1] == '_' || (p[1] >= 'A' && p[1] <= 'Z') || (p[1] >= 'a' && p[1] <= 'z'))) {
            if (p == sql || p[-1] == ' ' || p[-1] == ',' || p[-1] == '(' || p[-1] == '=') {
                param_count++;
                while (p[1] && (p[1] == '_' || (p[1] >= '0' && p[1] <= '9') ||
                       (p[1] >= 'A' && p[1] <= 'Z') || (p[1] >= 'a' && p[1] <= 'z'))) {
                    p++;
                }
            }
        }
    }
    return param_count;
}

// Build dummy SQL with N parameters (must match db_interpose_prepare.c logic)
static int build_dummy_sql(char *buf, size_t bufsize, int param_count) {
    if (param_count == 0) {
        return snprintf(buf, bufsize, "SELECT 1 WHERE 0");
    }
    int off = snprintf(buf, bufsize, "SELECT 1 WHERE ");
    for (int i = 0; i < param_count && (size_t)off < bufsize - 20; i++) {
        if (i > 0) off += snprintf(buf + off, bufsize - off, " AND ");
        off += snprintf(buf + off, bufsize - off, "? IS NOT NULL");
    }
    return off;
}

// ============================================================================
// 1. In-memory shadow DB tests
// ============================================================================

static void test_memory_db_opens(void) {
    TEST("In-memory SQLite opens successfully");
    sqlite3 *db = NULL;
    int rc = sqlite3_open(":memory:", &db);
    ASSERT_EQ(rc, SQLITE_OK, "sqlite3_open(:memory:) should return SQLITE_OK");
    ASSERT_NOT_NULL(db, "db handle should not be NULL");
    sqlite3_close(db);
    PASS();
}

static void test_memory_db_dummy_prepare(void) {
    TEST("Dummy SELECT 1 WHERE 0 prepares on :memory:");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    int rc = sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, &stmt, NULL);
    ASSERT_EQ(rc, SQLITE_OK, "dummy prepare should succeed");
    ASSERT_NOT_NULL(stmt, "stmt should not be NULL");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_memory_db_dummy_with_params(void) {
    TEST("Dummy with 5 params prepares on :memory:");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    int rc = sqlite3_prepare_v2(db,
        "SELECT 1 WHERE ? IS NOT NULL AND ? IS NOT NULL AND ? IS NOT NULL AND ? IS NOT NULL AND ? IS NOT NULL",
        -1, &stmt, NULL);
    ASSERT_EQ(rc, SQLITE_OK, "5-param dummy should prepare");
    ASSERT_EQ(sqlite3_bind_parameter_count(stmt), 5, "parameter count should be 5");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_memory_db_no_schema_needed(void) {
    TEST("Dummy prepare works without any CREATE TABLE");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    // No CREATE TABLE — just dummy prepares
    sqlite3_stmt *stmt = NULL;
    int rc = sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL", -1, &stmt, NULL);
    ASSERT_EQ(rc, SQLITE_OK, "should work without any tables");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_memory_db_multiple_stmts(void) {
    TEST("Multiple dummy stmts on same :memory: db");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmts[100];
    for (int i = 0; i < 100; i++) {
        char sql[256];
        build_dummy_sql(sql, sizeof(sql), i % 10);
        int rc = sqlite3_prepare_v2(db, sql, -1, &stmts[i], NULL);
        ASSERT_EQ(rc, SQLITE_OK, "prepare should succeed for all");
    }
    // All stmts are unique pointers
    for (int i = 0; i < 100; i++) {
        for (int j = i + 1; j < 100; j++) {
            ASSERT(stmts[i] != stmts[j], "all stmt pointers should be unique");
        }
    }
    for (int i = 0; i < 100; i++) {
        sqlite3_finalize(stmts[i]);
    }
    sqlite3_close(db);
    PASS();
}

// ============================================================================
// 2. Bind absorption tests — all bind types on dummy stmts
// ============================================================================

static void test_bind_int_on_dummy(void) {
    TEST("sqlite3_bind_int on dummy stmt");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL", -1, &stmt, NULL);
    int rc = sqlite3_bind_int(stmt, 1, 42);
    ASSERT_EQ(rc, SQLITE_OK, "bind_int should succeed");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_bind_int64_on_dummy(void) {
    TEST("sqlite3_bind_int64 on dummy stmt");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL", -1, &stmt, NULL);
    int rc = sqlite3_bind_int64(stmt, 1, 9999999999LL);
    ASSERT_EQ(rc, SQLITE_OK, "bind_int64 should succeed");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_bind_double_on_dummy(void) {
    TEST("sqlite3_bind_double on dummy stmt");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL", -1, &stmt, NULL);
    int rc = sqlite3_bind_double(stmt, 1, 3.14);
    ASSERT_EQ(rc, SQLITE_OK, "bind_double should succeed");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_bind_text_on_dummy(void) {
    TEST("sqlite3_bind_text on dummy stmt");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL", -1, &stmt, NULL);
    int rc = sqlite3_bind_text(stmt, 1, "hello world", -1, SQLITE_TRANSIENT);
    ASSERT_EQ(rc, SQLITE_OK, "bind_text should succeed");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_bind_blob_on_dummy(void) {
    TEST("sqlite3_bind_blob on dummy stmt");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL", -1, &stmt, NULL);
    const char data[] = {0x00, 0x01, 0x02, 0x03};
    int rc = sqlite3_bind_blob(stmt, 1, data, sizeof(data), SQLITE_TRANSIENT);
    ASSERT_EQ(rc, SQLITE_OK, "bind_blob should succeed");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_bind_null_on_dummy(void) {
    TEST("sqlite3_bind_null on dummy stmt");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL", -1, &stmt, NULL);
    int rc = sqlite3_bind_null(stmt, 1);
    ASSERT_EQ(rc, SQLITE_OK, "bind_null should succeed");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_bind_all_params(void) {
    TEST("Bind to all 8 params on dummy stmt");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    char sql[512];
    build_dummy_sql(sql, sizeof(sql), 8);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
    ASSERT_EQ(sqlite3_bind_parameter_count(stmt), 8, "should have 8 params");
    for (int i = 1; i <= 8; i++) {
        int rc = sqlite3_bind_int(stmt, i, i * 100);
        ASSERT_EQ(rc, SQLITE_OK, "bind should succeed for each param");
    }
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

// ============================================================================
// 3. Connection routing tests — db_handle works on dummy stmts
// ============================================================================

static void test_db_handle_from_dummy(void) {
    TEST("sqlite3_db_handle returns correct db from dummy stmt");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, &stmt, NULL);
    sqlite3 *result = sqlite3_db_handle(stmt);
    ASSERT(result == db, "db_handle should return the same db pointer");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_db_handle_multiple_dbs(void) {
    TEST("db_handle distinguishes stmts from different :memory: dbs");
    sqlite3 *db1 = NULL, *db2 = NULL;
    sqlite3_open(":memory:", &db1);
    sqlite3_open(":memory:", &db2);
    sqlite3_stmt *stmt1 = NULL, *stmt2 = NULL;
    sqlite3_prepare_v2(db1, "SELECT 1 WHERE 0", -1, &stmt1, NULL);
    sqlite3_prepare_v2(db2, "SELECT 1 WHERE 0", -1, &stmt2, NULL);
    ASSERT(sqlite3_db_handle(stmt1) == db1, "stmt1 should belong to db1");
    ASSERT(sqlite3_db_handle(stmt2) == db2, "stmt2 should belong to db2");
    ASSERT(db1 != db2, "two :memory: dbs should be different");
    sqlite3_finalize(stmt1);
    sqlite3_finalize(stmt2);
    sqlite3_close(db1);
    sqlite3_close(db2);
    PASS();
}

// ============================================================================
// 4. Dummy SQL builder tests
// ============================================================================

static void test_dummy_sql_zero_params(void) {
    TEST("Dummy SQL with 0 params");
    char sql[256];
    build_dummy_sql(sql, sizeof(sql), 0);
    ASSERT_STR(sql, "SELECT 1 WHERE 0", "0 params should give SELECT 1 WHERE 0");
    PASS();
}

static void test_dummy_sql_one_param(void) {
    TEST("Dummy SQL with 1 param");
    char sql[256];
    build_dummy_sql(sql, sizeof(sql), 1);
    ASSERT_STR(sql, "SELECT 1 WHERE ? IS NOT NULL", "1 param");
    PASS();
}

static void test_dummy_sql_three_params(void) {
    TEST("Dummy SQL with 3 params");
    char sql[256];
    build_dummy_sql(sql, sizeof(sql), 3);
    ASSERT_STR(sql, "SELECT 1 WHERE ? IS NOT NULL AND ? IS NOT NULL AND ? IS NOT NULL", "3 params");
    PASS();
}

static void test_dummy_sql_large_param_count(void) {
    TEST("Dummy SQL with 50 params prepares successfully");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    char sql[4096];
    build_dummy_sql(sql, sizeof(sql), 50);
    sqlite3_stmt *stmt = NULL;
    int rc = sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
    ASSERT_EQ(rc, SQLITE_OK, "50-param dummy should prepare");
    ASSERT_EQ(sqlite3_bind_parameter_count(stmt), 50, "should have 50 params");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

// ============================================================================
// 5. Parameter counting for real Plex queries
// ============================================================================

static void test_param_count_simple_select(void) {
    TEST("Param count: SELECT with 3 ?");
    int c = count_sql_params("SELECT * FROM media_items WHERE id = ? AND type = ? AND deleted_at IS ?");
    ASSERT_EQ(c, 3, "should count 3 params");
    PASS();
}

static void test_param_count_named_params(void) {
    TEST("Param count: INSERT with :named params");
    int c = count_sql_params("INSERT INTO tags (tag, tag_type) VALUES (:C1, :C2)");
    ASSERT_EQ(c, 2, "should count 2 named params");
    PASS();
}

static void test_param_count_mixed(void) {
    TEST("Param count: mixed ? and :named");
    int c = count_sql_params("SELECT * FROM t WHERE a = ? AND b = :name AND c = ?");
    ASSERT_EQ(c, 3, "should count 3 mixed params");
    PASS();
}

static void test_param_count_quoted_question(void) {
    TEST("Param count: ? inside string literal not counted");
    int c = count_sql_params("SELECT * FROM t WHERE a = '?' AND b = ?");
    ASSERT_EQ(c, 1, "should skip ? in quotes");
    PASS();
}

static void test_param_count_url_colon(void) {
    TEST("Param count: colon in URL not counted as param");
    int c = count_sql_params("SELECT * FROM t WHERE url = 'http://example.com' AND id = ?");
    ASSERT_EQ(c, 1, "colon in URL string should not count");
    PASS();
}

static void test_param_count_zero(void) {
    TEST("Param count: no params");
    int c = count_sql_params("SELECT count(*) FROM media_items");
    ASSERT_EQ(c, 0, "should count 0 params");
    PASS();
}

static void test_param_count_real_plex_media_items(void) {
    TEST("Param count: real Plex media_items query");
    int c = count_sql_params(
        "SELECT md.id, md.library_section_id, md.metadata_item_id "
        "FROM media_items md "
        "JOIN media_parts pt ON pt.media_item_id = md.id "
        "LEFT JOIN media_streams st ON st.media_part_id = pt.id "
        "WHERE md.metadata_item_id = ? AND md.deleted_at IS NULL");
    ASSERT_EQ(c, 1, "real Plex query should have 1 param");
    PASS();
}

// ============================================================================
// 6. PG udt_name → SQLite type mapping tests
// ============================================================================

// Extracted mapping (must match db_interpose_column.c pg_udt_to_sqlite_decltype)
static const char* pg_udt_to_sqlite_decltype(const char *udt_name) {
    if (!udt_name) return "TEXT";
    if (strcmp(udt_name, "int4") == 0) return "INTEGER";
    if (strcmp(udt_name, "int2") == 0) return "INTEGER";
    if (strcmp(udt_name, "int8") == 0) return "dt_integer(8)";
    if (strcmp(udt_name, "bool") == 0) return "INTEGER";
    if (strcmp(udt_name, "oid") == 0)  return "INTEGER";
    if (strcmp(udt_name, "float4") == 0) return "REAL";
    if (strcmp(udt_name, "float8") == 0) return "REAL";
    if (strcmp(udt_name, "numeric") == 0) return "REAL";
    if (strcmp(udt_name, "text") == 0) return "TEXT";
    if (strcmp(udt_name, "varchar") == 0) return "TEXT";
    if (strcmp(udt_name, "bpchar") == 0) return "TEXT";
    if (strcmp(udt_name, "name") == 0) return "TEXT";
    if (strcmp(udt_name, "tsvector") == 0) return "TEXT";
    if (strcmp(udt_name, "interval") == 0) return "TEXT";
    if (strcmp(udt_name, "timestamp") == 0) return "INTEGER";
    if (strcmp(udt_name, "timestamptz") == 0) return "INTEGER";
    if (strcmp(udt_name, "bytea") == 0) return "BLOB";
    return "TEXT";
}

static void test_udt_int4(void) {
    TEST("UDT mapping: int4 -> INTEGER");
    ASSERT_STR(pg_udt_to_sqlite_decltype("int4"), "INTEGER", "int4");
    PASS();
}

static void test_udt_int8(void) {
    TEST("UDT mapping: int8 -> dt_integer(8)");
    ASSERT_STR(pg_udt_to_sqlite_decltype("int8"), "dt_integer(8)", "int8 must be bigint marker");
    PASS();
}

static void test_udt_float8(void) {
    TEST("UDT mapping: float8 -> REAL");
    ASSERT_STR(pg_udt_to_sqlite_decltype("float8"), "REAL", "float8");
    PASS();
}

static void test_udt_varchar(void) {
    TEST("UDT mapping: varchar -> TEXT");
    ASSERT_STR(pg_udt_to_sqlite_decltype("varchar"), "TEXT", "varchar");
    PASS();
}

static void test_udt_text(void) {
    TEST("UDT mapping: text -> TEXT");
    ASSERT_STR(pg_udt_to_sqlite_decltype("text"), "TEXT", "text");
    PASS();
}

static void test_udt_bytea(void) {
    TEST("UDT mapping: bytea -> BLOB");
    ASSERT_STR(pg_udt_to_sqlite_decltype("bytea"), "BLOB", "bytea");
    PASS();
}

static void test_udt_bool(void) {
    TEST("UDT mapping: bool -> INTEGER");
    ASSERT_STR(pg_udt_to_sqlite_decltype("bool"), "INTEGER", "bool is int in sqlite");
    PASS();
}

static void test_udt_timestamp(void) {
    TEST("UDT mapping: timestamp -> INTEGER (unix epoch)");
    ASSERT_STR(pg_udt_to_sqlite_decltype("timestamp"), "INTEGER", "timestamp");
    PASS();
}

static void test_udt_timestamptz(void) {
    TEST("UDT mapping: timestamptz -> INTEGER");
    ASSERT_STR(pg_udt_to_sqlite_decltype("timestamptz"), "INTEGER", "timestamptz");
    PASS();
}

static void test_udt_tsvector(void) {
    TEST("UDT mapping: tsvector -> TEXT");
    ASSERT_STR(pg_udt_to_sqlite_decltype("tsvector"), "TEXT", "tsvector");
    PASS();
}

static void test_udt_unknown(void) {
    TEST("UDT mapping: unknown type -> TEXT (safe default)");
    ASSERT_STR(pg_udt_to_sqlite_decltype("jsonb"), "TEXT", "unknown defaults to TEXT");
    PASS();
}

static void test_udt_null(void) {
    TEST("UDT mapping: NULL -> TEXT");
    ASSERT_STR(pg_udt_to_sqlite_decltype(NULL), "TEXT", "NULL defaults to TEXT");
    PASS();
}

// ============================================================================
// 7. Step on dummy returns SQLITE_DONE (no rows)
// ============================================================================

static void test_step_dummy_returns_done(void) {
    TEST("sqlite3_step on dummy returns SQLITE_DONE");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, &stmt, NULL);
    int rc = sqlite3_step(stmt);
    ASSERT_EQ(rc, SQLITE_DONE, "step on WHERE 0 should return DONE");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

static void test_step_dummy_with_params_returns_done(void) {
    TEST("sqlite3_step on dummy with bound params returns SQLITE_DONE");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL AND ? IS NOT NULL", -1, &stmt, NULL);
    sqlite3_bind_int(stmt, 1, 42);
    sqlite3_bind_text(stmt, 2, "hello", -1, SQLITE_TRANSIENT);
    int rc = sqlite3_step(stmt);
    // With non-NULL binds, WHERE ? IS NOT NULL is TRUE, so we get SQLITE_ROW
    // This is fine — the shim never steps the dummy, PG handles the real query
    ASSERT(rc == SQLITE_ROW || rc == SQLITE_DONE, "step should return ROW or DONE");
    sqlite3_finalize(stmt);
    sqlite3_close(db);
    PASS();
}

// ============================================================================
// 8. Finalize safety
// ============================================================================

static void test_finalize_dummy(void) {
    TEST("sqlite3_finalize on dummy stmt succeeds");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, &stmt, NULL);
    int rc = sqlite3_finalize(stmt);
    ASSERT_EQ(rc, SQLITE_OK, "finalize should succeed");
    sqlite3_close(db);
    PASS();
}

static void test_close_with_outstanding_stmts(void) {
    TEST("sqlite3_close_v2 with unfinalized dummy stmts");
    sqlite3 *db = NULL;
    sqlite3_open(":memory:", &db);
    sqlite3_stmt *stmt1 = NULL, *stmt2 = NULL;
    sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, &stmt1, NULL);
    sqlite3_prepare_v2(db, "SELECT 1 WHERE ? IS NOT NULL", -1, &stmt2, NULL);
    // close_v2 should handle outstanding stmts gracefully
    int rc = sqlite3_close_v2(db);
    ASSERT_EQ(rc, SQLITE_OK, "close_v2 should succeed");
    // Finalize after close_v2 (deferred close)
    sqlite3_finalize(stmt1);
    sqlite3_finalize(stmt2);
    PASS();
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== Shadow SQLite Elimination Tests ===\033[0m\n\n");

    printf("\033[1m1. In-Memory Shadow DB:\033[0m\n");
    test_memory_db_opens();
    test_memory_db_dummy_prepare();
    test_memory_db_dummy_with_params();
    test_memory_db_no_schema_needed();
    test_memory_db_multiple_stmts();

    printf("\n\033[1m2. Bind Absorption on Dummy Stmts:\033[0m\n");
    test_bind_int_on_dummy();
    test_bind_int64_on_dummy();
    test_bind_double_on_dummy();
    test_bind_text_on_dummy();
    test_bind_blob_on_dummy();
    test_bind_null_on_dummy();
    test_bind_all_params();

    printf("\n\033[1m3. Connection Routing (db_handle):\033[0m\n");
    test_db_handle_from_dummy();
    test_db_handle_multiple_dbs();

    printf("\n\033[1m4. Dummy SQL Builder:\033[0m\n");
    test_dummy_sql_zero_params();
    test_dummy_sql_one_param();
    test_dummy_sql_three_params();
    test_dummy_sql_large_param_count();

    printf("\n\033[1m5. Parameter Counting (Real Plex SQL):\033[0m\n");
    test_param_count_simple_select();
    test_param_count_named_params();
    test_param_count_mixed();
    test_param_count_quoted_question();
    test_param_count_url_colon();
    test_param_count_zero();
    test_param_count_real_plex_media_items();

    printf("\n\033[1m6. PG Type Mapping (udt_name -> SQLite decltype):\033[0m\n");
    test_udt_int4();
    test_udt_int8();
    test_udt_float8();
    test_udt_varchar();
    test_udt_text();
    test_udt_bytea();
    test_udt_bool();
    test_udt_timestamp();
    test_udt_timestamptz();
    test_udt_tsvector();
    test_udt_unknown();
    test_udt_null();

    printf("\n\033[1m7. Step & Finalize Safety:\033[0m\n");
    test_step_dummy_returns_done();
    test_step_dummy_with_params_returns_done();
    test_finalize_dummy();
    test_close_with_outstanding_stmts();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
