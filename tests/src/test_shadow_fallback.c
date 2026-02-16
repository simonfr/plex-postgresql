/*
 * Unit tests for Shadow SQLite Dummy Fallback (v0.9.29)
 *
 * Tests the fix that prevents READ queries from failing when the shadow SQLite
 * schema is out of sync with PostgreSQL. When sqlite3_prepare_v2 fails on the
 * shadow database (e.g., "no such column: media_part_settings.changed_at"),
 * the shim now creates a dummy shadow statement with the correct number of
 * bind parameters so that:
 *
 * 1. sqlite3_bind_* calls succeed (correct parameter count)
 * 2. The query executes purely on PostgreSQL (shadow is never stepped)
 * 3. Plex doesn't see an error for queries that should work fine on PG
 *
 * The parameter counting logic must handle:
 * - Simple ? placeholders
 * - :named parameters (e.g., :C1, :name)
 * - ? inside string literals (should NOT be counted)
 * - : that is NOT a parameter (e.g., in URLs, timestamps)
 * - Mixed ? and :named in the same query
 * - Queries with zero parameters
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)
#define ASSERT_EQ(a, b, msg) do { if ((a) != (b)) { char buf[256]; snprintf(buf, sizeof(buf), "%s (got %d, expected %d)", msg, (a), (b)); FAIL(buf); return; } } while(0)

// ============================================================================
// Parameter counting logic (extracted from db_interpose_prepare.c)
// ============================================================================

// Count bind parameters in SQL: ? and :name
// Must skip ? and : inside string literals (single quotes)
// :name is only counted when preceded by space, comma, (, or =
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
        } else if (*p == ':' && p > sql) {
            char prev = *(p - 1);
            if (prev == ' ' || prev == ',' || prev == '(' || prev == '=') {
                param_count++;
            }
        }
    }
    return param_count;
}

// Build a dummy SQL statement with the right number of bind slots
// Returns a heap-allocated string. Caller must free.
static char* build_dummy_shadow_sql(int param_count) {
    char *sql = malloc(2048);
    if (!sql) return NULL;

    if (param_count == 0) {
        snprintf(sql, 2048, "SELECT 1 WHERE 0");
    } else {
        int off = snprintf(sql, 2048, "SELECT 1 WHERE ");
        for (int i = 0; i < param_count; i++) {
            if (i > 0) off += snprintf(sql + off, 2048 - off, " AND ");
            off += snprintf(sql + off, 2048 - off, "? IS NOT NULL");
            if (off >= 2028) break;  // Safety margin
        }
    }
    return sql;
}

// Count ? in generated dummy SQL (to verify it has the right number of bind slots)
static int count_question_marks(const char *sql) {
    int count = 0;
    for (const char *p = sql; *p; p++) {
        if (*p == '?') count++;
    }
    return count;
}

// ============================================================================
// Tests: Parameter counting
// ============================================================================

static void test_count_zero_params(void) {
    TEST("Zero parameters - simple SELECT");
    ASSERT_EQ(count_sql_params("SELECT * FROM metadata_items"), 0, "plain select");
    PASS();
}

static void test_count_question_mark_params(void) {
    TEST("? placeholder parameters");
    ASSERT_EQ(count_sql_params("SELECT * FROM foo WHERE id = ?"), 1, "one ?");
    ASSERT_EQ(count_sql_params("SELECT * FROM foo WHERE a = ? AND b = ?"), 2, "two ?");
    ASSERT_EQ(count_sql_params("INSERT INTO t VALUES (?, ?, ?, ?)"), 4, "four ?");
    PASS();
}

static void test_count_named_params(void) {
    TEST(":named parameters");
    ASSERT_EQ(count_sql_params("SELECT * FROM foo WHERE id = :C1"), 1, "one :named");
    ASSERT_EQ(count_sql_params("SELECT * FROM foo WHERE a = :C1 AND b = :C2"), 2, "two :named");
    PASS();
}

static void test_count_mixed_params(void) {
    TEST("Mixed ? and :named parameters");
    ASSERT_EQ(count_sql_params("SELECT * FROM foo WHERE a = ? AND b = :C1"), 2, "mixed");
    ASSERT_EQ(count_sql_params("SELECT * FROM foo WHERE a = :C1 AND b = ? AND c = :C2"), 3, "mixed 3");
    PASS();
}

static void test_count_params_in_quotes_ignored(void) {
    TEST("? inside string literals are NOT counted");
    ASSERT_EQ(count_sql_params("SELECT * FROM foo WHERE a = '?'"), 0, "? in quotes");
    ASSERT_EQ(count_sql_params("UPDATE t SET guid=REPLACE(guid,'?lang=en','?lang=xn')"), 0, "? in REPLACE strings");
    ASSERT_EQ(count_sql_params("SELECT * FROM foo WHERE a = '?' AND b = ?"), 1, "one real ? after quoted");
    PASS();
}

static void test_count_colon_not_param(void) {
    TEST(": that is NOT a parameter (URLs, timestamps)");
    // Colon after a letter (like in URLs) should not be counted
    ASSERT_EQ(count_sql_params("SELECT 'http://example.com' FROM foo"), 0, "URL colon");
    // Colon in HH:MM:SS
    ASSERT_EQ(count_sql_params("SELECT '12:30:00' FROM foo"), 0, "time colon");
    // :: PostgreSQL cast
    ASSERT_EQ(count_sql_params("SELECT id::text FROM foo"), 0, "PG cast");
    PASS();
}

static void test_count_colon_after_valid_prefix(void) {
    TEST(": after space/comma/(/) = is a parameter");
    ASSERT_EQ(count_sql_params("WHERE id = :id"), 1, "= :id");
    ASSERT_EQ(count_sql_params("WHERE id =:id"), 1, "=:id (no space)");
    ASSERT_EQ(count_sql_params("VALUES (:a, :b, :c)"), 3, "(:a, :b, :c)");
    PASS();
}

static void test_count_plex_real_query(void) {
    TEST("Real Plex queries with :C1 style parameters");
    const char *sql1 = "select media_items.id as 'media_items_id' "
                        "from media_items "
                        "WHERE media_items.metadata_item_id = :C1 "
                        "AND media_items.library_section_id = :C2";
    ASSERT_EQ(count_sql_params(sql1), 2, "Plex media_items query");

    // Plex bulk query with many params
    const char *sql2 = "SELECT * FROM metadata_items "
                        "WHERE id IN (:C1, :C2, :C3, :C4, :C5)";
    ASSERT_EQ(count_sql_params(sql2), 5, "Plex IN clause");
    PASS();
}

static void test_count_empty_and_null(void) {
    TEST("Empty and NULL SQL strings");
    ASSERT_EQ(count_sql_params(""), 0, "empty string");
    ASSERT_EQ(count_sql_params(NULL), 0, "NULL");
    PASS();
}

// ============================================================================
// Tests: Dummy shadow SQL generation
// ============================================================================

static void test_dummy_zero_params(void) {
    TEST("Dummy SQL with 0 parameters = 'SELECT 1 WHERE 0'");
    char *sql = build_dummy_shadow_sql(0);
    if (!sql) { FAIL("malloc failed"); return; }
    if (strcmp(sql, "SELECT 1 WHERE 0") != 0) { FAIL("wrong SQL"); free(sql); return; }
    if (count_question_marks(sql) != 0) { FAIL("should have 0 ?"); free(sql); return; }
    free(sql);
    PASS();
}

static void test_dummy_one_param(void) {
    TEST("Dummy SQL with 1 parameter");
    char *sql = build_dummy_shadow_sql(1);
    if (!sql) { FAIL("malloc failed"); return; }
    ASSERT_EQ(count_question_marks(sql), 1, "should have 1 ?");
    if (strstr(sql, "? IS NOT NULL") == NULL) { FAIL("should contain '? IS NOT NULL'"); free(sql); return; }
    free(sql);
    PASS();
}

static void test_dummy_five_params(void) {
    TEST("Dummy SQL with 5 parameters");
    char *sql = build_dummy_shadow_sql(5);
    if (!sql) { FAIL("malloc failed"); return; }
    ASSERT_EQ(count_question_marks(sql), 5, "should have 5 ?");
    // Verify structure: SELECT 1 WHERE ? IS NOT NULL AND ? IS NOT NULL ...
    if (strncmp(sql, "SELECT 1 WHERE ", 15) != 0) { FAIL("wrong prefix"); free(sql); return; }
    free(sql);
    PASS();
}

static void test_dummy_many_params(void) {
    TEST("Dummy SQL with 50 parameters (large Plex join)");
    char *sql = build_dummy_shadow_sql(50);
    if (!sql) { FAIL("malloc failed"); return; }
    ASSERT_EQ(count_question_marks(sql), 50, "should have 50 ?");
    free(sql);
    PASS();
}

// ============================================================================
// Tests: End-to-end parameter matching
// ============================================================================

static void test_e2e_real_plex_query(void) {
    TEST("E2E: Real Plex query → correct dummy parameter count");

    // This is the actual query that caused the original bug
    const char *plex_sql = "select media_items.id as 'media_items_id', "
                           "media_items.library_section_id as 'media_items_library_section_id' "
                           "from media_items "
                           "inner join media_parts on media_parts.media_item_id = media_items.id "
                           "inner join media_part_settings on media_part_settings.media_part_id = media_parts.id "
                           "WHERE media_part_settings.account_id = :C1 "
                           "AND media_items.library_section_id IN (:C2, :C3, :C4, :C5, :C6)";

    int params = count_sql_params(plex_sql);
    ASSERT_EQ(params, 6, "should detect 6 parameters");

    char *dummy = build_dummy_shadow_sql(params);
    if (!dummy) { FAIL("malloc failed"); return; }
    ASSERT_EQ(count_question_marks(dummy), 6, "dummy should have 6 bind slots");
    free(dummy);
    PASS();
}

static void test_e2e_no_param_query(void) {
    TEST("E2E: Query without parameters → dummy with no bind slots");

    const char *sql = "select max(media_part_settings.changed_at) from media_part_settings";
    int params = count_sql_params(sql);
    ASSERT_EQ(params, 0, "should detect 0 parameters");

    char *dummy = build_dummy_shadow_sql(params);
    if (!dummy) { FAIL("malloc failed"); return; }
    if (strcmp(dummy, "SELECT 1 WHERE 0") != 0) { FAIL("should be 'SELECT 1 WHERE 0'"); free(dummy); return; }
    free(dummy);
    PASS();
}

static void test_e2e_query_with_quoted_question_marks(void) {
    TEST("E2E: Query with ? in strings → only real params counted");

    const char *sql = "UPDATE metadata_items SET guid=REPLACE(guid,'?lang=en','?lang=xn') "
                      "WHERE id = ? AND library_section_id = ?";
    int params = count_sql_params(sql);
    ASSERT_EQ(params, 2, "should only count real ? outside quotes");

    char *dummy = build_dummy_shadow_sql(params);
    if (!dummy) { FAIL("malloc failed"); return; }
    ASSERT_EQ(count_question_marks(dummy), 2, "dummy should have 2 bind slots");
    free(dummy);
    PASS();
}

// ============================================================================
// Tests: Edge cases
// ============================================================================

static void test_edge_single_char_sql(void) {
    TEST("Edge: single character SQL strings");
    ASSERT_EQ(count_sql_params("?"), 1, "just ?");
    ASSERT_EQ(count_sql_params("x"), 0, "just x");
    PASS();
}

static void test_edge_consecutive_params(void) {
    TEST("Edge: consecutive ? without spaces");
    // This is unusual SQL but should still count correctly
    ASSERT_EQ(count_sql_params("SELECT ?,?,?"), 3, "?,?,?");
    PASS();
}

static void test_edge_colon_at_start(void) {
    TEST("Edge: :param at the very start of SQL");
    // At position 0, p > sql is false, so : at start is NOT counted
    ASSERT_EQ(count_sql_params(":param"), 0, "leading :param");
    // But after a space it is
    ASSERT_EQ(count_sql_params(" :param"), 1, "space then :param");
    PASS();
}

static void test_edge_nested_quotes(void) {
    TEST("Edge: escaped quotes in strings");
    // SQLite uses '' for escaped single quote inside string
    ASSERT_EQ(count_sql_params("SELECT 'it''s a ? test'"), 0, "escaped quote with ?");
    // The ? after the closing quote should be counted
    ASSERT_EQ(count_sql_params("SELECT 'hello' WHERE x = ?"), 1, "? after string");
    PASS();
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== Shadow SQLite Dummy Fallback Tests (v0.9.29) ===\033[0m\n");

    printf("\n\033[1mParameter Counting:\033[0m\n");
    test_count_zero_params();
    test_count_question_mark_params();
    test_count_named_params();
    test_count_mixed_params();
    test_count_params_in_quotes_ignored();
    test_count_colon_not_param();
    test_count_colon_after_valid_prefix();
    test_count_plex_real_query();
    test_count_empty_and_null();

    printf("\n\033[1mDummy SQL Generation:\033[0m\n");
    test_dummy_zero_params();
    test_dummy_one_param();
    test_dummy_five_params();
    test_dummy_many_params();

    printf("\n\033[1mEnd-to-End (Query → Dummy):\033[0m\n");
    test_e2e_real_plex_query();
    test_e2e_no_param_query();
    test_e2e_query_with_quoted_question_marks();

    printf("\n\033[1mEdge Cases:\033[0m\n");
    test_edge_single_char_sql();
    test_edge_consecutive_params();
    test_edge_colon_at_start();
    test_edge_nested_quotes();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
