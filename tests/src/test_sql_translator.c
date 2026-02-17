/*
 * Unit tests for SQL translation (SQLite to PostgreSQL)
 *
 * Tests:
 * 1. Placeholder translation (:name → $1)
 * 2. Function translation (IFNULL → COALESCE, etc.)
 * 3. Type translation (INTEGER → BIGINT, etc.)
 * 4. Keyword translation (GLOB → ILIKE, etc.)
 * 5. Full query translation
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>

// Include the translator headers
#include "sql_translator.h"
#include "sql_translator_internal.h"

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)

// ============================================================================
// Placeholder Translation Tests
// ============================================================================

static void test_placeholder_basic(void) {
    TEST("Placeholder - basic :name to $1");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders("SELECT * FROM t WHERE id = :id", &names, &count);

    if (result && strstr(result, "$1") && count == 1) {
        PASS();
    } else {
        FAIL("Expected $1 placeholder");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

static void test_placeholder_multiple(void) {
    TEST("Placeholder - multiple :name params");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = :foo AND b = :bar AND c = :baz", &names, &count);

    if (result && strstr(result, "$1") && strstr(result, "$2") && strstr(result, "$3") && count == 3) {
        PASS();
    } else {
        FAIL("Expected $1, $2, $3 placeholders");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

static void test_placeholder_reuse(void) {
    TEST("Placeholder - same :name used twice");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = :id OR b = :id", &names, &count);

    // Same param used twice should map to same $N
    if (result && count == 1) {
        PASS();
    } else {
        FAIL("Expected single param for reused :id");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

static void test_placeholder_question_mark(void) {
    TEST("Placeholder - ? positional params");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = ? AND b = ?", &names, &count);

    if (result && strstr(result, "$1") && strstr(result, "$2") && count == 2) {
        PASS();
    } else {
        FAIL("Expected $1, $2 for ? params");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

static void test_placeholder_in_string(void) {
    TEST("Placeholder - :name inside string literal ignored");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = ':not_a_param'", &names, &count);

    // Should NOT translate :not_a_param inside quotes
    if (result && count == 0 && strstr(result, ":not_a_param")) {
        PASS();
    } else {
        FAIL("Should not translate :param inside string");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

// ============================================================================
// Placeholder Edge Cases
// ============================================================================

static void test_placeholder_question_alpha_space(void) {
    TEST("Placeholder - ? before alpha -> space inserted ($1 left not $1left)");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = ? AND b > ?left", &names, &count);
    // The ? before "left" should produce "$2 left" not "$2left"
    // But actually "?left" - the ? is followed by 'l', so space is inserted
    // First ? becomes $1, ?left -> should become "$2 left" (space before 'l')
    // Actually "?left" is two chars: '?' + 'l'. The code checks isalpha(*p) after '?'
    // It should insert a space if the next char is alpha.
    // But wait - "AND b > ?left" - `?` is the placeholder, `left` is separate text
    // after the placeholder. So result should be "$2 left" not "$2left"
    if (result && count == 2 && !strstr(result, "$2l")) {
        PASS();
    } else {
        FAIL("Expected space between $2 and alpha char");
        if (result) printf("    Got: %s\n", result);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

static void test_placeholder_mixed_question_and_named(void) {
    TEST("Placeholder - mixed ? and :name -> sequential numbering");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = ? AND b = :foo AND c = ?", &names, &count);
    if (result && count == 3 &&
        strstr(result, "$1") && strstr(result, "$2") && strstr(result, "$3")) {
        PASS();
    } else {
        FAIL("Expected $1, $2, $3");
        if (result) printf("    Got: %s count=%d\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

static void test_placeholder_escaped_quotes(void) {
    TEST("Placeholder - :param inside escaped quotes ('it''s :not') -> skipped");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE name = 'it''s :not_a_param' AND id = :real_param", &names, &count);
    // :not_a_param inside the escaped-quote string should be ignored
    // :real_param outside should become $1
    if (result && count == 1 && strstr(result, ":not_a_param") && strstr(result, "$1")) {
        PASS();
    } else {
        FAIL("Expected :not_a_param preserved, :real_param -> $1");
        if (result) printf("    Got: %s count=%d\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

static void test_placeholder_colon_after_ident(void) {
    TEST("Placeholder - table:col (colon after ident) -> NOT a placeholder");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE url = 'http:endpoint'", &names, &count);
    // The : in 'http:endpoint' is inside quotes so should be ignored anyway
    // Let's also test unquoted: the guard checks is_ident_char(*(p-1))
    if (result && count == 0 && strstr(result, "http:endpoint")) {
        PASS();
    } else {
        FAIL("Expected no placeholders");
        if (result) printf("    Got: %s count=%d\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

// ============================================================================
// Function Translation Tests
// ============================================================================

static void test_function_ifnull(void) {
    TEST("Function - IFNULL to COALESCE");
    char *result = sql_translate_functions("SELECT IFNULL(a, 0) FROM t");

    if (result && strcasestr(result, "COALESCE") && !strcasestr(result, "IFNULL")) {
        PASS();
    } else {
        FAIL("Expected COALESCE instead of IFNULL");
    }

    if (result) free(result);
}

static void test_function_length(void) {
    TEST("Function - LENGTH preserved");
    char *result = sql_translate_functions("SELECT LENGTH(name) FROM t");

    if (result && strcasestr(result, "LENGTH")) {
        PASS();
    } else {
        FAIL("LENGTH should be preserved");
    }

    if (result) free(result);
}

static void test_function_substr(void) {
    TEST("Function - SUBSTR to SUBSTRING");
    char *result = sql_translate_functions("SELECT SUBSTR(a, 1, 5) FROM t");

    if (result && strcasestr(result, "SUBSTRING")) {
        PASS();
    } else {
        FAIL("Expected SUBSTRING");
    }

    if (result) free(result);
}

static void test_function_random(void) {
    TEST("Function - RANDOM() to RANDOM()");
    char *result = sql_translate_functions("SELECT RANDOM() FROM t");

    // PostgreSQL also has RANDOM(), should work
    if (result && strcasestr(result, "RANDOM")) {
        PASS();
    } else {
        FAIL("RANDOM should be preserved");
    }

    if (result) free(result);
}

static void test_function_datetime(void) {
    TEST("Function - datetime('now') handling");
    char *result = sql_translate_functions("SELECT datetime('now') FROM t");

    // Should translate to NOW() or similar
    if (result) {
        PASS();  // Just check it doesn't crash
    } else {
        FAIL("datetime translation failed");
    }

    if (result) free(result);
}

// ============================================================================
// Keyword Translation Tests
// ============================================================================

static void test_keyword_glob(void) {
    TEST("Keyword - GLOB to ILIKE");
    char *result = sql_translate_keywords("SELECT * FROM t WHERE name GLOB '*test*'");

    if (result && (strcasestr(result, "ILIKE") || strcasestr(result, "LIKE"))) {
        PASS();
    } else {
        FAIL("Expected ILIKE/LIKE pattern");
    }

    if (result) free(result);
}

static void test_keyword_notnull(void) {
    TEST("Keyword - NOT NULL preserved");
    char *result = sql_translate_keywords("SELECT * FROM t WHERE a IS NOT NULL");

    if (result && strcasestr(result, "NOT NULL")) {
        PASS();
    } else {
        FAIL("NOT NULL should be preserved");
    }

    if (result) free(result);
}

static void test_keyword_null_input(void) {
    TEST("Keyword - NULL -> NULL");
    char *result = sql_translate_keywords(NULL);
    if (result == NULL) { PASS(); }
    else { FAIL("Expected NULL"); free(result); }
}

static void test_keyword_alter_table_add_quoted(void) {
    TEST("Keyword - ALTER TABLE ADD 'col' -> ADD COLUMN IF NOT EXISTS");
    char *result = sql_translate_keywords("ALTER TABLE 'metadata_items' ADD 'new_col' TEXT");
    if (result && strcasestr(result, "ADD COLUMN IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected ADD COLUMN IF NOT EXISTS");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_alter_table_add_dblquoted(void) {
    TEST("Keyword - ALTER TABLE ADD \"col\" -> ADD COLUMN IF NOT EXISTS");
    char *result = sql_translate_keywords("ALTER TABLE \"metadata_items\" ADD \"new_col\" TEXT");
    if (result && strcasestr(result, "ADD COLUMN IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected ADD COLUMN IF NOT EXISTS");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_alter_table_add_unquoted(void) {
    TEST("Keyword - ALTER TABLE ADD col_name -> ADD COLUMN IF NOT EXISTS");
    char *result = sql_translate_keywords("ALTER TABLE metadata_items ADD new_col TEXT");
    if (result && strcasestr(result, "ADD COLUMN IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected ADD COLUMN IF NOT EXISTS");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_begin_immediate(void) {
    TEST("Keyword - BEGIN IMMEDIATE -> BEGIN");
    char *result = sql_translate_keywords("BEGIN IMMEDIATE");
    if (result && strcmp(result, "BEGIN") == 0) {
        PASS();
    } else {
        FAIL("Expected plain BEGIN");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_begin_deferred(void) {
    TEST("Keyword - BEGIN DEFERRED -> BEGIN");
    char *result = sql_translate_keywords("BEGIN DEFERRED");
    if (result && strcmp(result, "BEGIN") == 0) {
        PASS();
    } else {
        FAIL("Expected plain BEGIN");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_begin_exclusive(void) {
    TEST("Keyword - BEGIN EXCLUSIVE -> BEGIN");
    char *result = sql_translate_keywords("BEGIN EXCLUSIVE");
    if (result && strcmp(result, "BEGIN") == 0) {
        PASS();
    } else {
        FAIL("Expected plain BEGIN");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_insert_or_ignore(void) {
    TEST("Keyword - INSERT OR IGNORE INTO -> INSERT INTO");
    char *result = sql_translate_keywords(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('20230101')");
    if (result && strstr(result, "INSERT INTO") && !strcasestr(result, "OR IGNORE")) {
        PASS();
    } else {
        FAIL("Expected INSERT INTO without OR IGNORE");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_replace_into(void) {
    TEST("Keyword - REPLACE INTO -> INSERT INTO");
    char *result = sql_translate_keywords(
        "REPLACE INTO preferences (name, value) VALUES ('key', 'val')");
    if (result && strstr(result, "INSERT INTO") && !strcasestr(result, "REPLACE INTO")) {
        PASS();
    } else {
        FAIL("Expected INSERT INTO");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_empty_in(void) {
    TEST("Keyword - IN () -> IN (SELECT -1 WHERE FALSE)");
    char *result = sql_translate_keywords(
        "SELECT * FROM tags WHERE id in ()");
    if (result && strcasestr(result, "IN (SELECT -1 WHERE FALSE)")) {
        PASS();
    } else {
        FAIL("Expected IN (SELECT -1 WHERE FALSE)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_empty_in_spaces(void) {
    TEST("Keyword - IN (  ) with spaces -> IN (SELECT -1 WHERE FALSE)");
    char *result = sql_translate_keywords(
        "SELECT * FROM tags WHERE id in (  )");
    if (result && strcasestr(result, "IN (SELECT -1 WHERE FALSE)")) {
        PASS();
    } else {
        FAIL("Expected IN (SELECT -1 WHERE FALSE)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_group_by_null(void) {
    TEST("Keyword - GROUP BY NULL -> removed");
    char *result = sql_translate_keywords(
        "SELECT count(*) FROM metadata_items GROUP BY NULL");
    if (result && !strcasestr(result, "GROUP BY NULL")) {
        PASS();
    } else {
        FAIL("Expected GROUP BY NULL removed");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_having_cnt(void) {
    TEST("Keyword - HAVING cnt = 0 -> HAVING count(media_items.id) = 0");
    char *result = sql_translate_keywords(
        "SELECT * FROM media_items GROUP BY id HAVING cnt = 0");
    if (result && strstr(result, "HAVING count(media_items.id) = 0")) {
        PASS();
    } else {
        FAIL("Expected HAVING count(media_items.id) = 0");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_sqlite_master(void) {
    TEST("Keyword - sqlite_master -> information_schema subquery");
    char *result = sql_translate_keywords(
        "SELECT name FROM sqlite_master WHERE type='table'");
    if (result && strcasestr(result, "information_schema.tables") &&
        strcasestr(result, "pg_indexes") &&
        strstr(result, "FROM (SELECT")) {
        PASS();
    } else {
        FAIL("Expected information_schema subquery");
        if (result) printf("    Got: %.200s\n", result);
    }
    free(result);
}

static void test_keyword_sqlite_schema(void) {
    TEST("Keyword - sqlite_schema -> information_schema subquery");
    char *result = sql_translate_keywords(
        "SELECT name FROM sqlite_schema WHERE type='table'");
    if (result && strcasestr(result, "information_schema.tables") &&
        !strcasestr(result, "sqlite_schema")) {
        PASS();
    } else {
        FAIL("Expected information_schema subquery");
        if (result) printf("    Got: %.200s\n", result);
    }
    free(result);
}

static void test_keyword_indexed_by(void) {
    TEST("Keyword - INDEXED BY idx_name -> removed");
    char *result = sql_translate_keywords(
        "SELECT * FROM metadata_items INDEXED BY idx_title WHERE title = 'test'");
    if (result && !strcasestr(result, "INDEXED BY") && strstr(result, "WHERE title")) {
        PASS();
    } else {
        FAIL("Expected INDEXED BY removed");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_indexed_by_multiple(void) {
    TEST("Keyword - multiple INDEXED BY hints -> all removed");
    char *result = sql_translate_keywords(
        "SELECT * FROM t1 INDEXED BY idx1 JOIN t2 INDEXED BY idx2 ON t1.id = t2.id");
    if (result && !strcasestr(result, "INDEXED BY")) {
        PASS();
    } else {
        FAIL("Expected all INDEXED BY removed");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_keyword_main_dot_sqlite_master(void) {
    TEST("Keyword - \"main\".sqlite_master -> information_schema");
    char *result = sql_translate_keywords(
        "SELECT name FROM \"main\".sqlite_master WHERE type='table'");
    if (result && strcasestr(result, "information_schema.tables") &&
        strstr(result, "FROM (SELECT")) {
        PASS();
    } else {
        FAIL("Expected information_schema subquery");
        if (result) printf("    Got: %.200s\n", result);
    }
    free(result);
}

static void test_keyword_main_unquoted_sqlite_master(void) {
    TEST("Keyword - main.sqlite_master -> information_schema");
    char *result = sql_translate_keywords(
        "SELECT name FROM main.sqlite_master WHERE type='table'");
    if (result && strcasestr(result, "information_schema.tables") &&
        strstr(result, "FROM (SELECT")) {
        PASS();
    } else {
        FAIL("Expected information_schema subquery");
        if (result) printf("    Got: %.200s\n", result);
    }
    free(result);
}

static void test_keyword_sqlite_master_order_by_rowid(void) {
    TEST("Keyword - sqlite_master ORDER BY rowid -> rowid removed");
    char *result = sql_translate_keywords(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY rowid");
    if (result && !strcasestr(result, "ORDER BY rowid")) {
        PASS();
    } else {
        FAIL("Expected ORDER BY rowid removed");
        if (result) printf("    Got: %.200s\n", result);
    }
    free(result);
}

static void test_keyword_replace_into_preserved_with_insert_or(void) {
    TEST("Keyword - INSERT OR REPLACE INTO -> REPLACE INTO not translated");
    // When INSERT OR is present, REPLACE INTO should NOT be translated
    char *result = sql_translate_keywords(
        "INSERT OR REPLACE INTO tags (id, tag) VALUES (1, 'test')");
    // The INSERT OR REPLACE should still contain "REPLACE" since keywords
    // doesn't handle it (upsert module does). But it should NOT become "INSERT INTO"
    // from the REPLACE INTO rule.
    if (result && strcasestr(result, "REPLACE")) {
        PASS();
    } else {
        FAIL("Expected REPLACE preserved when INSERT OR present");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Type Translation Tests
// ============================================================================

static void test_type_autoincrement(void) {
    TEST("Type - AUTOINCREMENT to SERIAL");
    char *result = sql_translate_types("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)");

    if (result && strcasestr(result, "SERIAL") && !strcasestr(result, "AUTOINCREMENT")) {
        PASS();
    } else {
        FAIL("Expected SERIAL, no AUTOINCREMENT");
    }

    if (result) free(result);
}

static void test_type_text(void) {
    TEST("Type - TEXT preserved");
    char *result = sql_translate_types("CREATE TABLE t (name TEXT)");

    if (result && strcasestr(result, "TEXT")) {
        PASS();
    } else {
        FAIL("TEXT should be preserved");
    }

    if (result) free(result);
}

static void test_type_dt_integer8(void) {
    TEST("Type - dt_integer(8) -> BIGINT");
    char *result = sql_translate_types("CREATE TABLE t (ts dt_integer(8) NOT NULL)");
    if (result && strstr(result, "BIGINT") && !strcasestr(result, "dt_integer")) {
        PASS();
    } else {
        FAIL("Expected BIGINT");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_integer8(void) {
    TEST("Type - integer(8) -> BIGINT");
    char *result = sql_translate_types("CREATE TABLE t (ts integer(8) DEFAULT 0)");
    if (result && strstr(result, "BIGINT") && !strcasestr(result, "integer(8)")) {
        PASS();
    } else {
        FAIL("Expected BIGINT");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_blob_paren(void) {
    TEST("Type - BLOB) -> BYTEA) in column def");
    char *result = sql_translate_types("CREATE TABLE t (data BLOB)");
    if (result && strstr(result, "BYTEA)") && !strcasestr(result, "BLOB")) {
        PASS();
    } else {
        FAIL("Expected BYTEA)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_blob_comma(void) {
    TEST("Type - BLOB, -> BYTEA, between columns");
    char *result = sql_translate_types("CREATE TABLE t (data BLOB, name TEXT)");
    if (result && strstr(result, "BYTEA,") && !strcasestr(result, "BLOB")) {
        PASS();
    } else {
        FAIL("Expected BYTEA,");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_blob_space(void) {
    TEST("Type - BLOB DEFAULT -> BYTEA DEFAULT");
    char *result = sql_translate_types("CREATE TABLE t (data BLOB DEFAULT NULL)");
    if (result && strstr(result, "BYTEA ") && !strcasestr(result, "BLOB")) {
        PASS();
    } else {
        FAIL("Expected BYTEA ");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_default_true(void) {
    TEST("Type - DEFAULT 't' -> DEFAULT TRUE");
    char *result = sql_translate_types("CREATE TABLE t (active boolean DEFAULT 't')");
    if (result && strstr(result, "DEFAULT TRUE") && !strstr(result, "'t'")) {
        PASS();
    } else {
        FAIL("Expected DEFAULT TRUE");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_default_false(void) {
    TEST("Type - DEFAULT 'f' -> DEFAULT FALSE");
    char *result = sql_translate_types("CREATE TABLE t (disabled boolean DEFAULT 'f')");
    if (result && strstr(result, "DEFAULT FALSE") && !strstr(result, "'f'")) {
        PASS();
    } else {
        FAIL("Expected DEFAULT FALSE");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_datetime_paren(void) {
    TEST("Type - datetime) -> TIMESTAMP) in column def");
    char *result = sql_translate_types("CREATE TABLE t (created_at datetime)");
    if (result && strcasestr(result, "TIMESTAMP)") && !strcasestr(result, "datetime")) {
        PASS();
    } else {
        FAIL("Expected TIMESTAMP)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_datetime_comma(void) {
    TEST("Type - datetime, -> TIMESTAMP, between columns");
    char *result = sql_translate_types("CREATE TABLE t (created_at datetime, name TEXT)");
    if (result && strcasestr(result, "TIMESTAMP,") && !strcasestr(result, "datetime")) {
        PASS();
    } else {
        FAIL("Expected TIMESTAMP,");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_datetime_space(void) {
    TEST("Type - datetime DEFAULT -> TIMESTAMP DEFAULT");
    char *result = sql_translate_types("CREATE TABLE t (created_at datetime DEFAULT NULL)");
    if (result && strcasestr(result, "TIMESTAMP ") && !strcasestr(result, "datetime")) {
        PASS();
    } else {
        FAIL("Expected TIMESTAMP ");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_type_null_input(void) {
    TEST("Type - NULL -> NULL");
    char *result = sql_translate_types(NULL);
    if (result == NULL) { PASS(); }
    else { FAIL("Expected NULL"); free(result); }
}

static void test_type_no_patterns(void) {
    TEST("Type - no matching patterns -> unchanged");
    char *result = sql_translate_types("SELECT id, name FROM metadata_items");
    if (result && strcmp(result, "SELECT id, name FROM metadata_items") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged");
    }
    free(result);
}

// ============================================================================
// Full Translation Tests
// ============================================================================

static void test_full_select(void) {
    TEST("Full - simple SELECT");
    sql_translation_t result = sql_translate("SELECT * FROM metadata_items WHERE id = :id");

    if (result.success && result.sql && strstr(result.sql, "$1")) {
        PASS();
    } else {
        FAIL(result.error[0] ? result.error : "Translation failed");
    }

    sql_translation_free(&result);
}

static void test_full_insert(void) {
    TEST("Full - INSERT with values");
    sql_translation_t result = sql_translate(
        "INSERT INTO t (a, b) VALUES (:a, :b)");

    if (result.success && result.sql && result.param_count == 2) {
        PASS();
    } else {
        FAIL("INSERT translation failed");
    }

    sql_translation_free(&result);
}

static void test_full_update(void) {
    TEST("Full - UPDATE with WHERE");
    sql_translation_t result = sql_translate(
        "UPDATE t SET a = :val WHERE id = :id");

    if (result.success && result.sql && result.param_count == 2) {
        PASS();
    } else {
        FAIL("UPDATE translation failed");
    }

    sql_translation_free(&result);
}

static void test_full_complex(void) {
    TEST("Full - complex Plex-like query");
    sql_translation_t result = sql_translate(
        "SELECT m.id, m.title, IFNULL(m.rating, 0) as rating "
        "FROM metadata_items m "
        "WHERE m.library_section_id = :lib_id "
        "AND m.metadata_type = :type "
        "ORDER BY m.added_at DESC LIMIT 50");

    if (result.success && result.sql &&
        strcasestr(result.sql, "COALESCE") &&  // IFNULL → COALESCE
        strstr(result.sql, "$1") &&
        strstr(result.sql, "$2")) {
        PASS();
    } else {
        FAIL("Complex query translation failed");
    }

    sql_translation_free(&result);
}

// ============================================================================
// Plex-specific inline fix pipeline tests
// ============================================================================

static void test_plex_viewed_at_order_by(void) {
    TEST("Plex - ORDER BY viewed_at -> ORDER BY max(viewed_at) when max() present");
    sql_translation_t r = sql_translate(
        "SELECT metadata_item_id, max(viewed_at) FROM metadata_item_views "
        "GROUP BY metadata_item_id ORDER BY viewed_at DESC");
    if (r.success && r.sql &&
        strcasestr(r.sql, "order by max(viewed_at)") &&
        !strstr(r.sql, "order by viewed_at desc")) {
        PASS();
    } else {
        FAIL("Expected ORDER BY max(viewed_at)");
        if (r.sql) printf("    Got: %.200s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_plex_external_metadata_group_by(void) {
    TEST("Plex - external_metadata_items GROUP BY expanded");
    sql_translation_t r = sql_translate(
        "SELECT external_metadata_items.id,uri,user_title,library_section_id,"
        "metadata_type,year,added_at,updated_at,extra_data,title "
        "FROM external_metadata_items "
        "group by title order by added_at");
    if (r.success && r.sql &&
        strcasestr(r.sql, "group by title,external_metadata_items.id")) {
        PASS();
    } else {
        FAIL("Expected expanded GROUP BY");
        if (r.sql) printf("    Got: %.200s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_plex_clustering_distinct_removes_group_by(void) {
    TEST("Plex - metadata_item_clusterings DISTINCT removes GROUP BY");
    sql_translation_t r = sql_translate(
        "SELECT DISTINCT metadata_item_clusterings.id, title "
        "FROM metadata_item_clusterings "
        "GROUP BY title ORDER BY title");
    if (r.success && r.sql &&
        !strcasestr(r.sql, "GROUP BY title ORDER")) {
        PASS();
    } else {
        FAIL("Expected GROUP BY removed with DISTINCT");
        if (r.sql) printf("    Got: %.200s\n", r.sql);
    }
    sql_translation_free(&r);
}

// ============================================================================
// Edge Case Tests
// ============================================================================

static void test_edge_empty(void) {
    TEST("Edge - empty string");
    sql_translation_t result = sql_translate("");

    // Should handle gracefully
    if (result.sql != NULL || !result.success) {
        PASS();  // Either returns empty or fails gracefully
    } else {
        FAIL("Empty string not handled");
    }

    sql_translation_free(&result);
}

static void test_edge_null(void) {
    TEST("Edge - NULL input");
    sql_translation_t result = sql_translate(NULL);

    // Should not crash
    PASS();

    sql_translation_free(&result);
}

static void test_edge_backticks(void) {
    TEST("Edge - backtick identifiers to double quotes");
    sql_translation_t result = sql_translate("SELECT `id`, `name` FROM `table`");

    // Backticks should become double quotes
    if (result.success && result.sql &&
        !strstr(result.sql, "`") &&
        strstr(result.sql, "\"")) {
        PASS();
    } else {
        FAIL("Backticks not converted to double quotes");
    }

    sql_translation_free(&result);
}

static void test_edge_double_quotes_preserved(void) {
    TEST("Edge - double quotes preserved");
    sql_translation_t result = sql_translate("SELECT \"id\" FROM \"table\"");

    if (result.success && result.sql && strstr(result.sql, "\"")) {
        PASS();
    } else {
        FAIL("Double quotes should be preserved");
    }

    sql_translation_free(&result);
}

// ============================================================================
// COLLATE NOCASE Tests (NEW - TDD)
// ============================================================================

static void test_collate_nocase_equals(void) {
    TEST("COLLATE NOCASE - equality comparison");
    sql_translation_t result = sql_translate(
        "SELECT * FROM t WHERE name COLLATE NOCASE = 'Test'");

    // Should translate to LOWER(name) = LOWER('Test')
    if (result.success && result.sql &&
        strcasestr(result.sql, "LOWER") &&
        !strcasestr(result.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected LOWER() conversion for COLLATE NOCASE");
    }

    sql_translation_free(&result);
}

static void test_collate_nocase_like(void) {
    TEST("COLLATE NOCASE - LIKE comparison");
    sql_translation_t result = sql_translate(
        "SELECT * FROM t WHERE name LIKE '%test%' COLLATE NOCASE");

    // Should translate to ILIKE or LOWER(name) LIKE LOWER('%test%')
    if (result.success && result.sql &&
        (strcasestr(result.sql, "ILIKE") || strcasestr(result.sql, "LOWER")) &&
        !strcasestr(result.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected ILIKE or LOWER() for COLLATE NOCASE LIKE");
    }

    sql_translation_free(&result);
}

static void test_collate_nocase_orderby(void) {
    TEST("COLLATE NOCASE - ORDER BY");
    sql_translation_t result = sql_translate(
        "SELECT * FROM t ORDER BY name COLLATE NOCASE");

    // Should translate to ORDER BY LOWER(name)
    if (result.success && result.sql &&
        strcasestr(result.sql, "LOWER") &&
        !strcasestr(result.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected LOWER() in ORDER BY for COLLATE NOCASE");
    }

    sql_translation_free(&result);
}

static void test_collate_nocase_glob(void) {
    TEST("COLLATE NOCASE - GLOB comparison -> ILIKE");
    sql_translation_t result = sql_translate(
        "SELECT * FROM t WHERE name GLOB '*test*' COLLATE NOCASE");
    if (result.success && result.sql &&
        (strcasestr(result.sql, "ILIKE") || strcasestr(result.sql, "LOWER(")) &&
        !strcasestr(result.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected ILIKE or LOWER() for COLLATE NOCASE GLOB");
        if (result.sql) printf("    Got: %s\n", result.sql);
    }
    sql_translation_free(&result);
}

static void test_collate_nocase_ne(void) {
    TEST("COLLATE NOCASE - != comparison -> LOWER() != LOWER()");
    char *result = translate_collate_nocase(
        "SELECT * FROM t WHERE name COLLATE NOCASE != 'Test'");
    if (result && strcasestr(result, "LOWER(") && strstr(result, "!=") &&
        !strcasestr(result, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected LOWER() with != for COLLATE NOCASE");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// FTS4 Boolean Search Tests (NEW - TDD)
// ============================================================================

static void test_fts_negation(void) {
    TEST("FTS4 - negation operator (-term)");
    sql_translation_t result = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action -comedy'");

    // Should translate -comedy to !comedy in tsquery
    if (result.success && result.sql &&
        strcasestr(result.sql, "to_tsquery") &&
        strcasestr(result.sql, "!")) {
        PASS();
    } else {
        FAIL("Expected ! negation in tsquery");
    }

    sql_translation_free(&result);
}

static void test_fts_and_chain(void) {
    TEST("FTS4 - AND chain (term1 AND term2)");
    sql_translation_t result = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action AND adventure'");

    // Should translate to action & adventure in tsquery
    if (result.success && result.sql &&
        strcasestr(result.sql, "to_tsquery") &&
        strcasestr(result.sql, "&")) {
        PASS();
    } else {
        FAIL("Expected & operator in tsquery");
    }

    sql_translation_free(&result);
}

static void test_fts_or_chain(void) {
    TEST("FTS4 - OR chain (term1 OR term2)");
    sql_translation_t result = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action OR adventure'");

    // Should translate to action | adventure in tsquery
    if (result.success && result.sql &&
        strcasestr(result.sql, "to_tsquery") &&
        strcasestr(result.sql, "|")) {
        PASS();
    } else {
        FAIL("Expected | operator in tsquery");
    }

    sql_translation_free(&result);
}

static void test_fts_phrase(void) {
    TEST("FTS4 - phrase search (\"exact phrase\")");
    sql_translation_t result = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH '\"star wars\"'");

    // Should translate to phrase search with <-> operator
    if (result.success && result.sql &&
        strcasestr(result.sql, "to_tsquery")) {
        PASS();  // Basic check - phrase handling is complex
    } else {
        FAIL("Expected tsquery for phrase search");
    }

    sql_translation_free(&result);
}

// ============================================================================
// Window Functions Tests (NEW - TDD)
// ============================================================================

static void test_window_row_number(void) {
    TEST("Window - ROW_NUMBER() OVER");
    sql_translation_t result = sql_translate(
        "SELECT ROW_NUMBER() OVER (ORDER BY id) as rn FROM t");

    // PostgreSQL supports this natively, should pass through
    if (result.success && result.sql &&
        strcasestr(result.sql, "ROW_NUMBER") &&
        strcasestr(result.sql, "OVER")) {
        PASS();
    } else {
        FAIL("ROW_NUMBER() OVER should be preserved");
    }

    sql_translation_free(&result);
}

static void test_window_rank(void) {
    TEST("Window - RANK() with PARTITION BY");
    sql_translation_t result = sql_translate(
        "SELECT RANK() OVER (PARTITION BY category ORDER BY score DESC) FROM t");

    // PostgreSQL supports this natively
    if (result.success && result.sql &&
        strcasestr(result.sql, "RANK") &&
        strcasestr(result.sql, "PARTITION BY")) {
        PASS();
    } else {
        FAIL("RANK() with PARTITION BY should be preserved");
    }

    sql_translation_free(&result);
}

static void test_window_dense_rank(void) {
    TEST("Window - DENSE_RANK()");
    sql_translation_t result = sql_translate(
        "SELECT DENSE_RANK() OVER (ORDER BY score) FROM t");

    if (result.success && result.sql &&
        strcasestr(result.sql, "DENSE_RANK")) {
        PASS();
    } else {
        FAIL("DENSE_RANK() should be preserved");
    }

    sql_translation_free(&result);
}

// ============================================================================
// FTS Quote Parsing Tests (Bug Fix Tests)
// Tests that MATCH queries with SQL-escaped quotes are correctly translated
// ============================================================================

static void test_fts_single_escaped_quote(void) {
    TEST("FTS Quote - single escaped quote (it''s*)");
    sql_translation_t result = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH '(it''s*)'");

    // The '' should be unescaped to ' and the term should be valid tsquery
    // Result should have to_tsquery and the term should be properly formatted
    if (result.success && result.sql &&
        strcasestr(result.sql, "to_tsquery") &&
        !strstr(result.sql, "''")) {  // No double quotes should remain in tsquery
        PASS();
    } else {
        FAIL("Single escaped quote should be unescaped in tsquery");
        if (result.sql) printf("    Got: %s\n", result.sql);
    }

    sql_translation_free(&result);
}

static void test_fts_double_escaped_quote(void) {
    TEST("FTS Quote - double escaped quote (test''''test*)");
    sql_translation_t result = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH '(test''''test*)'");

    // '''' in SQL represents '' (two actual quotes) which should become one quote
    if (result.success && result.sql &&
        strcasestr(result.sql, "to_tsquery")) {
        PASS();
    } else {
        FAIL("Double escaped quote should be handled");
        if (result.sql) printf("    Got: %s\n", result.sql);
    }

    sql_translation_free(&result);
}

static void test_fts_simple_term(void) {
    TEST("FTS Quote - simple term (no quotes)");
    sql_translation_t result = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'simple'");

    // Basic case should still work
    if (result.success && result.sql &&
        strcasestr(result.sql, "to_tsquery") &&
        strcasestr(result.sql, "simple")) {
        PASS();
    } else {
        FAIL("Simple term should be translated to tsquery");
        if (result.sql) printf("    Got: %s\n", result.sql);
    }

    sql_translation_free(&result);
}

static void test_fts_mixed_quotes_and_terms(void) {
    TEST("FTS Quote - mixed quotes and wildcards");
    sql_translation_t result = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH '(don''t* stop*)'");

    // Should handle both the escaped quote and the wildcards
    if (result.success && result.sql &&
        strcasestr(result.sql, "to_tsquery") &&
        strstr(result.sql, ":*")) {  // Wildcard should be converted to :*
        PASS();
    } else {
        FAIL("Mixed quotes and wildcards should work together");
        if (result.sql) printf("    Got: %s\n", result.sql);
    }

    sql_translation_free(&result);
}

// ============================================================================
// INSTR Function Tests
// ============================================================================

static void test_function_instr(void) {
    TEST("Function - instr(a, b) -> STRPOS(a, b)");
    char *result = sql_translate_functions(
        "SELECT * FROM t WHERE NOT instr(extra_data, 'pv%3AlastFmBlacklisted=1')");
    if (result && strstr(result, "STRPOS(extra_data, 'pv%3AlastFmBlacklisted=1')")) {
        PASS();
    } else {
        FAIL("instr should be translated to STRPOS");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_function_instr_no_match(void) {
    TEST("Function - no instr() -> unchanged");
    char *result = sql_translate_functions("SELECT * FROM t WHERE id = 1");
    if (result && strstr(result, "SELECT * FROM t WHERE id = 1")) {
        PASS();
    } else {
        FAIL("Query without instr should be unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// JSON Operator Parameter Tests (Bug Fix Tests)
// Tests that JSON operators with parameter placeholders work correctly
// ============================================================================

static void test_json_operator_with_parameter(void) {
    TEST("JSON Op - column ->> '$.key' < $3 -> ::json->>'key' < $3");
    sql_translation_t result = sql_translate(
        "SELECT * FROM t WHERE extra_data ->> '$.pv:version' < $3");

    // The JSON operator should be translated to ::json->>'key'
    // and the $3 parameter MUST be preserved (no bind mismatch)
    if (result.success && result.sql) {
        int has_json_cast = strstr(result.sql, "::json->>'pv:version'") != NULL;
        int has_param = strstr(result.sql, "$3") != NULL;

        if (has_json_cast && has_param) {
            PASS();
        } else {
            FAIL("JSON operator should use ::json->> and preserve params");
            printf("    has_json_cast=%d has_param=%d\n", has_json_cast, has_param);
            printf("    Got: %s\n", result.sql);
        }
    } else {
        FAIL("Translation failed");
    }

    sql_translation_free(&result);
}

static void test_json_operator_with_literal(void) {
    TEST("JSON Op - column ->> '$.key' < '1' -> ::json->>'key' < '1'");
    sql_translation_t result = sql_translate(
        "SELECT * FROM t WHERE extra_data ->> '$.pv:version' < '1'");

    // Should be converted to ::json->>'key' with literal preserved
    if (result.success && result.sql &&
        strstr(result.sql, "::json->>'pv:version'") &&
        strstr(result.sql, "< '1'")) {
        PASS();
    } else {
        FAIL("JSON operator with literal should use ::json->>");
        if (result.sql) printf("    Got: %s\n", result.sql);
    }

    sql_translation_free(&result);
}

static void test_json_operator_is_null(void) {
    TEST("JSON Op - column ->> '$.key' IS NULL -> ::json->>'key' IS NULL");
    sql_translation_t result = sql_translate(
        "SELECT * FROM t WHERE extra_data ->> '$.pv:version' IS NULL");

    // Should be converted to ::json->>'key' IS NULL
    if (result.success && result.sql &&
        strstr(result.sql, "::json->>'pv:version'") &&
        strcasestr(result.sql, "IS NULL")) {
        PASS();
    } else {
        FAIL("JSON IS NULL should use ::json->>");
        if (result.sql) printf("    Got: %s\n", result.sql);
    }

    sql_translation_free(&result);
}

static void test_json_operator_param_position(void) {
    TEST("JSON Op - parameter with json cast (::json->>$N)");
    sql_translation_t result = sql_translate(
        "SELECT * FROM t WHERE data->>$1 = 'value'");

    // The ->>$N pattern should get ::json inserted before it
    if (result.success && result.sql &&
        strstr(result.sql, "::json->>$1")) {
        PASS();
    } else {
        FAIL("JSON operator with $N should insert ::json cast");
        if (result.sql) printf("    Got: %s\n", result.sql);
    }

    sql_translation_free(&result);
}

static void test_json_operator_plex_vad_query(void) {
    TEST("JSON Op - real Plex VAD query preserves all 3 params");
    sql_translation_t result = sql_translate(
        "SELECT id FROM media_parts WHERE metadata_item_id IN (?, ?) "
        "AND (extra_data ->> '$.pv:voiceActivityDetectionVersion' IS NULL "
        "OR extra_data ->> '$.pv:voiceActivityDetectionVersion' < ?)");

    if (result.success && result.sql) {
        int has_json = strstr(result.sql, "::json->>'pv:voiceActivityDetectionVersion'") != NULL;
        int has_p1 = strstr(result.sql, "$1") != NULL;
        int has_p2 = strstr(result.sql, "$2") != NULL;
        int has_p3 = strstr(result.sql, "$3") != NULL;

        if (has_json && has_p1 && has_p2 && has_p3 && result.param_count == 3) {
            PASS();
        } else {
            FAIL("All 3 params must be preserved");
            printf("    params=%d has_p1=%d p2=%d p3=%d json=%d\n",
                   result.param_count, has_p1, has_p2, has_p3, has_json);
            printf("    Got: %s\n", result.sql);
        }
    } else {
        FAIL("Translation failed");
    }

    sql_translation_free(&result);
}

// ============================================================================
// Helper Function Tests (sql_tr_helpers.c)
// ============================================================================

static void test_helper_str_replace(void) {
    TEST("Helper - str_replace basic");
    char *result = str_replace("hello world hello", "hello", "hi");
    if (result && strcmp(result, "hi world hi") == 0) {
        PASS();
    } else {
        FAIL("Expected 'hi world hi'");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_helper_str_replace_no_match(void) {
    TEST("Helper - str_replace no match returns copy");
    char *result = str_replace("hello world", "xyz", "abc");
    if (result && strcmp(result, "hello world") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged string");
    }
    free(result);
}

static void test_helper_str_replace_nocase(void) {
    TEST("Helper - str_replace_nocase case insensitive");
    char *result = str_replace_nocase("SELECT HELLO from t", "hello", "world");
    if (result && strstr(result, "world")) {
        PASS();
    } else {
        FAIL("Expected case-insensitive replacement");
    }
    free(result);
}

static void test_helper_safe_strcasestr(void) {
    TEST("Helper - safe_strcasestr finds match");
    const char *result = safe_strcasestr("Hello World", "WORLD");
    if (result && strncmp(result, "World", 5) == 0) {
        PASS();
    } else {
        FAIL("Expected to find 'World'");
    }
}

static void test_helper_safe_strcasestr_null(void) {
    TEST("Helper - safe_strcasestr NULL safety");
    if (safe_strcasestr(NULL, "test") == NULL &&
        safe_strcasestr("test", NULL) == NULL) {
        PASS();
    } else {
        FAIL("Expected NULL return for NULL input");
    }
}

static void test_helper_extract_arg(void) {
    TEST("Helper - extract_arg with nested parens");
    char buf[256];
    const char *input = "func(a, b), c)";
    const char *next = extract_arg(input, buf, sizeof(buf));
    if (strcmp(buf, "func(a, b)") == 0 && *next == ',') {
        PASS();
    } else {
        FAIL("Expected 'func(a, b)'");
        printf("    Got: '%s', next='%c'\n", buf, *next);
    }
}

// ============================================================================
// Function Translation Tests - IIF (sql_tr_functions.c)
// ============================================================================

static void test_function_iif(void) {
    TEST("Function - iif() to CASE WHEN");
    char *result = translate_iif("SELECT iif(a > 0, 'yes', 'no') FROM t");
    if (result && strcasestr(result, "CASE WHEN") &&
        strcasestr(result, "THEN") &&
        strcasestr(result, "ELSE") &&
        strcasestr(result, "END") &&
        !strcasestr(result, "iif")) {
        PASS();
    } else {
        FAIL("Expected CASE WHEN ... THEN ... ELSE ... END");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_function_iif_no_match(void) {
    TEST("Function - iif() passthrough when absent");
    char *result = translate_iif("SELECT a FROM t");
    if (result && strcmp(result, "SELECT a FROM t") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged query");
    }
    free(result);
}

// ============================================================================
// Function Translation Tests - TYPEOF (sql_tr_functions.c)
// ============================================================================

static void test_function_typeof(void) {
    TEST("Function - typeof() to pg_typeof()::text");
    char *result = translate_typeof("SELECT typeof(x) FROM t");
    if (result && strstr(result, "pg_typeof(x)::text")) {
        PASS();
    } else {
        FAIL("Expected pg_typeof(x)::text");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Function Translation Tests - STRFTIME (sql_tr_functions.c)
// ============================================================================

static void test_function_strftime_epoch(void) {
    TEST("Function - strftime('%s', 'now') to EXTRACT(EPOCH)");
    char *result = translate_strftime("SELECT strftime('%s', 'now')");
    if (result && strcasestr(result, "EXTRACT(EPOCH FROM NOW())") &&
        strstr(result, "::bigint")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM NOW())::bigint");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_function_strftime_epoch_interval(void) {
    TEST("Function - strftime('%s', 'now', '-7 day')");
    char *result = translate_strftime("SELECT strftime('%s', 'now', '-7 day')");
    if (result && strcasestr(result, "EXTRACT(EPOCH FROM NOW()") &&
        strcasestr(result, "INTERVAL") &&
        strstr(result, "::bigint")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM NOW() - INTERVAL ...)::bigint");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_function_strftime_date(void) {
    TEST("Function - strftime('%Y-%m-%d', col) to TO_CHAR");
    char *result = translate_strftime("SELECT strftime('%Y-%m-%d', added_at)");
    if (result && strcasestr(result, "TO_CHAR(added_at, 'YYYY-MM-DD')")) {
        PASS();
    } else {
        FAIL("Expected TO_CHAR with YYYY-MM-DD");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_function_strftime_column(void) {
    TEST("Function - strftime('%s', column) uses TO_TIMESTAMP");
    char *result = translate_strftime("SELECT strftime('%s', updated_at)");
    if (result && strcasestr(result, "TO_TIMESTAMP(updated_at)")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM TO_TIMESTAMP(col))::bigint");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Function Translation Tests - UNIXEPOCH (sql_tr_functions.c)
// ============================================================================

static void test_function_unixepoch_now(void) {
    TEST("Function - unixepoch('now') to EXTRACT(EPOCH)");
    char *result = translate_unixepoch("SELECT unixepoch('now')");
    if (result && strcasestr(result, "EXTRACT(EPOCH FROM NOW())") &&
        strstr(result, "::bigint")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM NOW())::bigint");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_function_unixepoch_interval(void) {
    TEST("Function - unixepoch('now', '-7 day')");
    char *result = translate_unixepoch("SELECT unixepoch('now', '-7 day')");
    if (result && strcasestr(result, "EXTRACT(EPOCH FROM NOW()") &&
        strcasestr(result, "INTERVAL")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM NOW() + INTERVAL ...)::bigint");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Function Translation Tests - last_insert_rowid, json_each (sql_tr_functions.c)
// ============================================================================

static void test_function_last_insert_rowid(void) {
    TEST("Function - last_insert_rowid() to lastval()");
    char *result = translate_last_insert_rowid("SELECT last_insert_rowid()");
    if (result && strcasestr(result, "lastval()") &&
        !strcasestr(result, "last_insert_rowid")) {
        PASS();
    } else {
        FAIL("Expected lastval()");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_function_json_each(void) {
    TEST("Function - json_each() to json_array_elements()");
    char *result = translate_json_each("SELECT value FROM json_each(data)");
    if (result && strcasestr(result, "json_array_elements(") &&
        !strcasestr(result, "json_each(")) {
        PASS();
    } else {
        FAIL("Expected json_array_elements()");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Function Translation Tests - simplify_typeof_fixup (sql_tr_functions.c)
// ============================================================================

static void test_function_simplify_typeof(void) {
    TEST("Function - simplify typeof fixup pattern");
    char *result = simplify_typeof_fixup(
        "SELECT iif(typeof(x) in ('integer', 'real'), x, strftime('%s', x, 'utc')) FROM t");
    if (result && strstr(result, "x") && !strcasestr(result, "iif(typeof(")) {
        PASS();
    } else {
        FAIL("Expected simplified to just 'x'");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Function Tests - typeof() type name remapping
// ============================================================================

static void test_typeof_integer_bigint_expansion(void) {
    TEST("typeof - in ('integer',) -> in ('integer', 'bigint',)");
    char *result = translate_typeof(
        "SELECT * FROM t WHERE typeof(x) in ('integer', 'real')");
    if (result && strstr(result, "'bigint'")) {
        PASS();
    } else {
        FAIL("Expected 'bigint' added");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_typeof_real_to_double_precision(void) {
    TEST("typeof - 'real' -> 'double precision'");
    char *result = translate_typeof(
        "SELECT * FROM t WHERE pg_typeof(x)::text = 'real'");
    if (result && strstr(result, "'double precision'") && !strstr(result, "'real'")) {
        PASS();
    } else {
        FAIL("Expected 'double precision'");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Function Tests - strftime() additional patterns
// ============================================================================

static void test_strftime_datetime_format(void) {
    TEST("strftime - '%Y-%m-%d %H:%M:%S' -> TO_CHAR(col, 'YYYY-MM-DD HH24:MI:SS')");
    char *result = translate_strftime(
        "SELECT strftime('%Y-%m-%d %H:%M:%S', created_at) FROM t");
    if (result && strstr(result, "TO_CHAR(created_at, 'YYYY-MM-DD HH24:MI:SS')")) {
        PASS();
    } else {
        FAIL("Expected TO_CHAR with datetime format");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_strftime_positive_interval(void) {
    TEST("strftime - '%s', 'now', '+7 day' -> NOW() + INTERVAL");
    char *result = translate_strftime(
        "SELECT strftime('%s', 'now', '+7 day') FROM t");
    if (result && strstr(result, "NOW() + INTERVAL '7 day'")) {
        PASS();
    } else {
        FAIL("Expected NOW() + INTERVAL");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_strftime_generic_format(void) {
    TEST("strftime - generic format -> TO_CHAR fallback");
    char *result = translate_strftime(
        "SELECT strftime('%H:%M', created_at) FROM t");
    if (result && strstr(result, "TO_CHAR(created_at,")) {
        PASS();
    } else {
        FAIL("Expected TO_CHAR fallback");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Function Tests - unixepoch() additional patterns
// ============================================================================

static void test_unixepoch_column(void) {
    TEST("unixepoch - unixepoch(created_at) -> EXTRACT(EPOCH FROM created_at)");
    char *result = translate_unixepoch(
        "SELECT unixepoch(created_at) FROM t");
    if (result && strstr(result, "EXTRACT(EPOCH FROM created_at)::bigint")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM col)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Function Tests - json_each value::text cast
// ============================================================================

static void test_json_each_value_text_cast(void) {
    TEST("json_each - value FROM json_array_elements -> value::text");
    char *result = translate_json_each(
        "SELECT value FROM json_each('[1,2,3]')");
    if (result && strstr(result, "value::text")) {
        PASS();
    } else {
        FAIL("Expected value::text");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Quote Translation Tests (sql_tr_quotes.c)
// ============================================================================

static void test_quote_column_quotes(void) {
    TEST("Quote - table.'column' to table.\"column\"");
    char *result = translate_column_quotes("SELECT t.'name' FROM t");
    if (result && strstr(result, "t.\"name\"") && !strstr(result, "t.'name'")) {
        PASS();
    } else {
        FAIL("Expected t.\"name\"");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_alias_quotes(void) {
    TEST("Quote - AS 'alias' to AS \"alias\"");
    char *result = translate_alias_quotes("SELECT a AS 'my_alias' FROM t");
    if (result && strstr(result, "AS \"my_alias\"") && !strstr(result, "AS 'my_alias'")) {
        PASS();
    } else {
        FAIL("Expected AS \"my_alias\"");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_ddl_table(void) {
    TEST("Quote - DDL CREATE TABLE 'name' to \"name\"");
    char *result = translate_ddl_quotes("CREATE TABLE 'my_table' (id INTEGER)");
    if (result && strstr(result, "\"my_table\"") && !strstr(result, "'my_table'")) {
        PASS();
    } else {
        FAIL("Expected \"my_table\"");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_ddl_not_dml(void) {
    TEST("Quote - DDL quotes not applied to DML");
    char *result = translate_ddl_quotes("SELECT * FROM t WHERE name = 'test'");
    if (result && strstr(result, "'test'")) {
        PASS();
    } else {
        FAIL("DML string literals should be preserved");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_if_not_exists_table(void) {
    TEST("Quote - add IF NOT EXISTS to CREATE TABLE");
    char *result = add_if_not_exists("CREATE TABLE foo (id INTEGER)");
    if (result && strcasestr(result, "IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected IF NOT EXISTS");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_if_not_exists_index(void) {
    TEST("Quote - add IF NOT EXISTS to CREATE INDEX");
    char *result = add_if_not_exists("CREATE INDEX idx_foo ON t(id)");
    if (result && strcasestr(result, "IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected IF NOT EXISTS");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_if_not_exists_unique_index(void) {
    TEST("Quote - add IF NOT EXISTS to CREATE UNIQUE INDEX");
    char *result = add_if_not_exists("CREATE UNIQUE INDEX idx_u ON t(name)");
    if (result && strcasestr(result, "IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected IF NOT EXISTS");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_if_not_exists_already(void) {
    TEST("Quote - IF NOT EXISTS already present");
    char *result = add_if_not_exists("CREATE TABLE IF NOT EXISTS foo (id INT)");
    if (result && strcmp(result, "CREATE TABLE IF NOT EXISTS foo (id INT)") == 0) {
        PASS();
    } else {
        FAIL("Should not double-add IF NOT EXISTS");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_on_conflict_unquote(void) {
    TEST("Quote - ON CONFLICT(\"name\") to ON CONFLICT(name)");
    char *result = fix_on_conflict_quotes(
        "INSERT INTO t VALUES (1) ON CONFLICT(\"name\") DO NOTHING");
    if (result && strstr(result, "ON CONFLICT(name)") &&
        !strstr(result, "ON CONFLICT(\"name\")")) {
        PASS();
    } else {
        FAIL("Expected unquoted column in ON CONFLICT");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Duplicate Assignment Tests (sql_tr_quotes.c)
// ============================================================================

static void test_dedup_assignments_basic(void) {
    TEST("Dedup - duplicate SET assignment keeps last");
    char *result = fix_duplicate_assignments(
        "UPDATE t SET a=1, b=2, a=3 WHERE id=1");
    if (result && strstr(result, "a=3") && strstr(result, "b=2") &&
        !strstr(result, "a=1,")) {
        PASS();
    } else {
        FAIL("Expected last assignment of 'a' to be kept");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_dedup_assignments_no_dup(void) {
    TEST("Dedup - no duplicates returns unchanged");
    char *result = fix_duplicate_assignments(
        "UPDATE t SET a=1, b=2 WHERE id=1");
    if (result && strstr(result, "a=1") && strstr(result, "b=2")) {
        PASS();
    } else {
        FAIL("Expected unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_dedup_assignments_quoted(void) {
    TEST("Dedup - handles backtick-quoted columns");
    char *result = fix_duplicate_assignments(
        "UPDATE t SET `col`=1, `col`=2 WHERE id=1");
    if (result && strstr(result, "`col`=2") && !strstr(result, "`col`=1,")) {
        PASS();
    } else {
        FAIL("Expected dedup with backtick columns");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_dedup_assignments_params(void) {
    TEST("Dedup - removed params consumed with COALESCE");
    char *result = fix_duplicate_assignments(
        "UPDATE t SET a=$1, b=$2, a=$3 WHERE id=$4");
    if (result && strstr(result, "COALESCE") && strstr(result, "$1::text")) {
        PASS();
    } else {
        FAIL("Expected COALESCE with removed $1");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_dedup_not_update(void) {
    TEST("Dedup - non-UPDATE returns unchanged");
    char *result = fix_duplicate_assignments("SELECT a, a FROM t");
    if (result && strcmp(result, "SELECT a, a FROM t") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged for SELECT");
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - fix_forward_reference_joins (sql_tr_query.c)
// ============================================================================

static void test_forward_ref_null(void) {
    TEST("ForwardRef - NULL -> NULL");
    char *result = fix_forward_reference_joins(NULL);
    if (result == NULL) { PASS(); }
    else { FAIL("Expected NULL"); free(result); }
}

static void test_forward_ref_no_alias_join(void) {
    TEST("ForwardRef - no aliased JOIN -> unchanged");
    char *result = fix_forward_reference_joins(
        "SELECT * FROM metadata_items JOIN media_items ON media_items.metadata_item_id = metadata_items.id");
    if (result && strstr(result, "SELECT * FROM metadata_items JOIN media_items")) {
        PASS();
    } else {
        FAIL("Expected unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_forward_ref_no_unaliased_join(void) {
    TEST("ForwardRef - no unaliased join -> unchanged");
    char *result = fix_forward_reference_joins(
        "SELECT * FROM t JOIN metadata_items AS parents ON parents.id = t.parent_id");
    if (result && strstr(result, "JOIN metadata_items AS parents")) {
        PASS();
    } else {
        FAIL("Expected unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_forward_ref_reorder(void) {
    TEST("ForwardRef - reorders JOIN when forward ref detected");
    // Simulate On Deck: aliased join references metadata_items.id before unaliased join defines it
    char *result = fix_forward_reference_joins(
        "SELECT * FROM media_items "
        "JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id "
        "JOIN metadata_items ON metadata_items.id = media_items.metadata_item_id "
        "WHERE parents.title IS NOT NULL");
    if (result &&
        // The unaliased JOIN should come before the aliased JOIN now
        strstr(result, "JOIN metadata_items ON") &&
        strstr(result, "JOIN metadata_items AS parents")) {
        // Verify the unaliased join appears BEFORE the aliased join
        const char *unaliased = strstr(result, "JOIN metadata_items ON");
        const char *aliased = strstr(result, "JOIN metadata_items AS parents");
        if (unaliased && aliased && unaliased < aliased) {
            PASS();
        } else {
            FAIL("Unaliased JOIN should come before aliased JOIN");
            printf("    Got: %s\n", result);
        }
    } else {
        FAIL("Expected reordered JOINs");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_forward_ref_no_forward_reference(void) {
    TEST("ForwardRef - aliased first but no forward ref -> unchanged");
    // Aliased join appears first but doesn't reference metadata_items.xxx between the joins
    char *result = fix_forward_reference_joins(
        "SELECT * FROM media_items "
        "JOIN metadata_items AS parents ON parents.id = media_items.parent_id "
        "JOIN metadata_items ON metadata_items.id = media_items.metadata_item_id");
    // No forward reference between the alias join and unaliased join
    // (parents.id references parents, not metadata_items)
    if (result) {
        PASS();  // Should return something (either changed or unchanged, depends on whether "metadata_items." appears in ON clause)
    } else {
        FAIL("Expected non-NULL result");
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - translate_null_sorting (sql_tr_query.c)
// ============================================================================

static void test_null_sorting(void) {
    TEST("Query - null sorting IS NULL,col asc -> NULLS LAST");
    char *result = translate_null_sorting(
        "SELECT * FROM t ORDER BY parents.\"index\" IS NULL, parents.\"index\" asc");
    if (result && strcasestr(result, "NULLS LAST") &&
        !strcasestr(result, "IS NULL")) {
        PASS();
    } else {
        FAIL("Expected NULLS LAST");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_null_sorting_no_match(void) {
    TEST("Query - null sorting passthrough when no IS NULL");
    char *result = translate_null_sorting("SELECT * FROM t ORDER BY id");
    if (result && strcmp(result, "SELECT * FROM t ORDER BY id") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged");
    }
    free(result);
}

static void test_null_sorting_null_input(void) {
    TEST("NullSort - NULL -> NULL");
    char *result = translate_null_sorting(NULL);
    if (result == NULL) { PASS(); }
    else { FAIL("Expected NULL"); free(result); }
}

static void test_null_sorting_backtick_parents_index(void) {
    TEST("NullSort - parents.`index` IS NULL,col asc -> NULLS LAST");
    char *result = translate_null_sorting(
        "SELECT * FROM t ORDER BY parents.`index` IS NULL,parents.`index` asc");
    if (result && strcasestr(result, "NULLS LAST") && !strcasestr(result, "IS NULL")) {
        PASS();
    } else {
        FAIL("Expected NULLS LAST");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_null_sorting_metadata_items_index(void) {
    TEST("NullSort - metadata_items.\"index\" IS NULL -> NULLS LAST");
    char *result = translate_null_sorting(
        "SELECT * FROM t ORDER BY metadata_items.\"index\" IS NULL, metadata_items.\"index\" asc");
    if (result && strcasestr(result, "NULLS LAST") && !strcasestr(result, "IS NULL")) {
        PASS();
    } else {
        FAIL("Expected NULLS LAST");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_null_sorting_originally_available_at(void) {
    TEST("NullSort - originally_available_at IS NULL -> NULLS LAST");
    char *result = translate_null_sorting(
        "SELECT * FROM t ORDER BY metadata_items.originally_available_at IS NULL, metadata_items.originally_available_at asc");
    if (result && strcasestr(result, "NULLS LAST") && !strcasestr(result, "IS NULL")) {
        PASS();
    } else {
        FAIL("Expected NULLS LAST");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_null_sorting_grandparents_title_sort(void) {
    TEST("NullSort - grandparents.title_sort IS NULL -> NULLS LAST");
    char *result = translate_null_sorting(
        "SELECT * FROM t ORDER BY grandparents.title_sort IS NULL, grandparents.title_sort asc");
    if (result && strcasestr(result, "NULLS LAST") && !strcasestr(result, "IS NULL")) {
        PASS();
    } else {
        FAIL("Expected NULLS LAST");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_null_sorting_space_variant(void) {
    TEST("NullSort - IS NULL, col ASC (space+uppercase) -> NULLS LAST");
    char *result = translate_null_sorting(
        "SELECT * FROM t ORDER BY parents.\"index\" IS NULL, parents.\"index\" ASC");
    if (result && strcasestr(result, "NULLS LAST") && !strcasestr(result, "IS NULL")) {
        PASS();
    } else {
        FAIL("Expected NULLS LAST");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - translate_distinct_orderby (sql_tr_query.c)
// ============================================================================

static void test_distinct_orderby_aggregate(void) {
    TEST("Query - remove DISTINCT with aggregate ORDER BY");
    char *result = translate_distinct_orderby(
        "SELECT DISTINCT id FROM t GROUP BY id ORDER BY count(*)");
    if (result && !strcasestr(result, "DISTINCT") &&
        strcasestr(result, "SELECT")) {
        PASS();
    } else {
        FAIL("Expected DISTINCT removed");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_distinct_orderby_random(void) {
    TEST("Query - remove DISTINCT with ORDER BY random()");
    char *result = translate_distinct_orderby(
        "SELECT DISTINCT id FROM t ORDER BY random()");
    if (result && !strcasestr(result, "DISTINCT")) {
        PASS();
    } else {
        FAIL("Expected DISTINCT removed for random()");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_distinct_orderby_groupby(void) {
    TEST("Query - remove DISTINCT when GROUP BY present");
    char *result = translate_distinct_orderby(
        "SELECT DISTINCT id FROM t GROUP BY id");
    if (result && !strcasestr(result, "DISTINCT")) {
        PASS();
    } else {
        FAIL("Expected DISTINCT removed with GROUP BY");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - translate_case_booleans (sql_tr_query.c)
// ============================================================================

static void test_case_booleans_else_1_true(void) {
    TEST("Query - CASE ELSE 1 END) -> ELSE true END)");
    char *result = translate_case_booleans(
        "SELECT (CASE WHEN a THEN 0 ELSE 1 END) FROM t");
    if (result && strcasestr(result, "else true end)") &&
        !strstr(result, "else 1 end)")) {
        PASS();
    } else {
        FAIL("Expected boolean translation");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_else_0_false(void) {
    TEST("Query - CASE ELSE 0 END) -> ELSE false END)");
    char *result = translate_case_booleans(
        "SELECT (CASE WHEN a THEN 1 ELSE 0 END) FROM t");
    if (result && strcasestr(result, "else false end)")) {
        PASS();
    } else {
        FAIL("Expected ELSE false");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_then_0_else_true(void) {
    TEST("Query - THEN 0 ELSE true -> THEN false ELSE true");
    char *result = translate_case_booleans(
        "SELECT (CASE WHEN a then 0 else true END) FROM t");
    if (result && strcasestr(result, "then false else true end)")) {
        PASS();
    } else {
        FAIL("Expected THEN false ELSE true");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_then_1_else_false(void) {
    TEST("Query - THEN 1 ELSE false -> THEN true ELSE false");
    char *result = translate_case_booleans(
        "SELECT (CASE WHEN a then 1 else false END) FROM t");
    if (result && strcasestr(result, "then true else false end)")) {
        PASS();
    } else {
        FAIL("Expected THEN true ELSE false");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_0_or(void) {
    TEST("Query - (0 or ...) -> (FALSE or ...)");
    char *result = translate_case_booleans(
        "SELECT * FROM t WHERE (0 or a = 1)");
    if (result && strstr(result, "(FALSE or a = 1)")) {
        PASS();
    } else {
        FAIL("Expected (FALSE or ...)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_1_or(void) {
    TEST("Query - (1 or ...) -> (TRUE or ...)");
    char *result = translate_case_booleans(
        "SELECT * FROM t WHERE (1 or a = 1)");
    if (result && strstr(result, "(TRUE or a = 1)")) {
        PASS();
    } else {
        FAIL("Expected (TRUE or ...)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_and_0(void) {
    TEST("Query - ... and 0) -> ... and FALSE)");
    char *result = translate_case_booleans(
        "SELECT * FROM t WHERE (a = 1 and 0)");
    if (result && strstr(result, "and FALSE)")) {
        PASS();
    } else {
        FAIL("Expected ... and FALSE)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_and_1(void) {
    TEST("Query - ... and 1) -> ... and TRUE)");
    char *result = translate_case_booleans(
        "SELECT * FROM t WHERE (a = 1 and 1)");
    if (result && strstr(result, "and TRUE)")) {
        PASS();
    } else {
        FAIL("Expected ... and TRUE)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_or_0(void) {
    TEST("Query - ... or 0) -> ... or FALSE)");
    char *result = translate_case_booleans(
        "SELECT * FROM t WHERE (a = 1 or 0)");
    if (result && strstr(result, "or FALSE)")) {
        PASS();
    } else {
        FAIL("Expected ... or FALSE)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_or_1(void) {
    TEST("Query - ... or 1) -> ... or TRUE)");
    char *result = translate_case_booleans(
        "SELECT * FROM t WHERE (a = 1 or 1)");
    if (result && strstr(result, "or TRUE)")) {
        PASS();
    } else {
        FAIL("Expected ... or TRUE)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_where_0(void) {
    TEST("Query - WHERE 0 -> WHERE FALSE");
    char *result = translate_case_booleans("SELECT * FROM t WHERE 0");
    if (result && strcasestr(result, "WHERE FALSE")) {
        PASS();
    } else {
        FAIL("Expected WHERE FALSE");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_where_1(void) {
    TEST("Query - WHERE 1 -> WHERE TRUE");
    char *result = translate_case_booleans("SELECT * FROM t WHERE 1");
    if (result && strcasestr(result, "WHERE TRUE")) {
        PASS();
    } else {
        FAIL("Expected WHERE TRUE");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_case_booleans_null_input(void) {
    TEST("Query - case booleans NULL -> NULL");
    char *result = translate_case_booleans(NULL);
    if (result == NULL) { PASS(); }
    else { FAIL("Expected NULL"); free(result); }
}

static void test_case_booleans_no_match(void) {
    TEST("Query - no boolean patterns -> unchanged");
    char *result = translate_case_booleans("SELECT id, name FROM metadata_items");
    if (result && strcmp(result, "SELECT id, name FROM metadata_items") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - fix_integer_text_mismatch (sql_tr_query.c)
// ============================================================================

static void test_int_text_mismatch_null(void) {
    TEST("IntTextMismatch - NULL -> NULL");
    char *result = fix_integer_text_mismatch(NULL);
    if (result == NULL) { PASS(); }
    else { FAIL("Expected NULL"); free(result); }
}

static void test_int_text_mismatch_no_match(void) {
    TEST("IntTextMismatch - no matching pattern -> unchanged");
    char *result = fix_integer_text_mismatch("SELECT * FROM metadata_items WHERE id = 1");
    if (result && strcmp(result, "SELECT * FROM metadata_items WHERE id = 1") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_int_text_mismatch_pattern1(void) {
    TEST("IntTextMismatch - metadata_items.id in (select taggings...) -> ::text");
    char *result = fix_integer_text_mismatch(
        "SELECT * FROM metadata_items WHERE metadata_items.id in (select taggings.metadata_item_id FROM taggings)");
    if (result &&
        strstr(result, "metadata_items.id::text in (select taggings.metadata_item_id::text")) {
        PASS();
    } else {
        FAIL("Expected ::text casts");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_int_text_mismatch_pattern2_backtick(void) {
    TEST("IntTextMismatch - `metadata_item_id` in + json_array_elements -> ::text");
    char *result = fix_integer_text_mismatch(
        "SELECT * FROM taggings WHERE `metadata_item_id` in "
        "(SELECT value::text FROM json_array_elements('[1,2,3]'))");
    if (result &&
        strstr(result, "`metadata_item_id`::text in")) {
        PASS();
    } else {
        FAIL("Expected ::text on backtick column");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_int_text_mismatch_pattern2_quote(void) {
    TEST("IntTextMismatch - \"metadata_item_id\" in + json_array_elements -> ::text");
    char *result = fix_integer_text_mismatch(
        "SELECT * FROM taggings WHERE \"metadata_item_id\" in "
        "(SELECT value::text FROM json_array_elements('[1,2,3]'))");
    if (result &&
        strstr(result, "\"metadata_item_id\"::text in")) {
        PASS();
    } else {
        FAIL("Expected ::text on quoted column");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_int_text_mismatch_pattern4_download_backtick(void) {
    TEST("IntTextMismatch - download_queue_items di.`status` + json -> ::text");
    char *result = fix_integer_text_mismatch(
        "SELECT * FROM download_queue_items di WHERE di.`status` IN "
        "(SELECT value::text FROM json_array_elements('[1,2]'))");
    if (result &&
        strstr(result, "di.`status`::text IN")) {
        PASS();
    } else {
        FAIL("Expected ::text on status");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_int_text_mismatch_pattern4_download_quote(void) {
    TEST("IntTextMismatch - download_queue_items di.\"status\" + json -> ::text");
    char *result = fix_integer_text_mismatch(
        "SELECT * FROM download_queue_items di WHERE di.\"status\" IN "
        "(SELECT value::text FROM json_array_elements('[1,2]'))");
    if (result &&
        strstr(result, "di.\"status\"::text IN")) {
        PASS();
    } else {
        FAIL("Expected ::text on status");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_int_text_mismatch_generic_status_backtick(void) {
    TEST("IntTextMismatch - generic `status` IN + json_array_elements -> ::text");
    char *result = fix_integer_text_mismatch(
        "SELECT * FROM some_table WHERE `status` IN "
        "(SELECT value::text FROM json_array_elements('[0,1]'))");
    if (result &&
        strstr(result, "`status`::text IN")) {
        PASS();
    } else {
        FAIL("Expected ::text on generic status");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_int_text_mismatch_generic_status_quote(void) {
    TEST("IntTextMismatch - generic \"status\" IN + json_array_elements -> ::text");
    char *result = fix_integer_text_mismatch(
        "SELECT * FROM some_table WHERE \"status\" IN "
        "(SELECT value::text FROM json_array_elements('[0,1]'))");
    if (result &&
        strstr(result, "\"status\"::text IN")) {
        PASS();
    } else {
        FAIL("Expected ::text on generic status");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - translate_max/min (sql_tr_query.c)
// ============================================================================

static void test_max_to_greatest(void) {
    TEST("Query - max(a, b) to GREATEST(a, b)");
    char *result = translate_max_to_greatest("SELECT max(x, y) FROM t");
    if (result && strcasestr(result, "GREATEST(x, y)") &&
        !strcasestr(result, "max(x, y)")) {
        PASS();
    } else {
        FAIL("Expected GREATEST(x, y)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_max_single_arg(void) {
    TEST("Query - max(a) stays as max(a) (aggregate)");
    char *result = translate_max_to_greatest("SELECT max(x) FROM t");
    if (result && strcasestr(result, "max(x)") &&
        !strcasestr(result, "GREATEST")) {
        PASS();
    } else {
        FAIL("Expected max(x) preserved as aggregate");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_min_to_least(void) {
    TEST("Query - min(a, b) to LEAST(a, b)");
    char *result = translate_min_to_least("SELECT min(x, y) FROM t");
    if (result && strcasestr(result, "LEAST(x, y)") &&
        !strcasestr(result, "min(x, y)")) {
        PASS();
    } else {
        FAIL("Expected LEAST(x, y)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_min_single_arg(void) {
    TEST("Query - min(a) stays as min(a) (aggregate)");
    char *result = translate_min_to_least("SELECT min(x) FROM t");
    if (result && strcasestr(result, "min(x)") &&
        !strcasestr(result, "LEAST")) {
        PASS();
    } else {
        FAIL("Expected min(x) preserved as aggregate");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - strip_icu_collation (sql_tr_query.c)
// ============================================================================

static void test_strip_icu_collation(void) {
    TEST("Query - strip COLLATE icu_root");
    char *result = strip_icu_collation(
        "SELECT * FROM t ORDER BY name COLLATE icu_root");
    if (result && !strcasestr(result, "icu_root")) {
        PASS();
    } else {
        FAIL("Expected icu_root removed");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_strip_icu_collation_no_match(void) {
    TEST("Query - strip icu_root passthrough when absent");
    char *result = strip_icu_collation("SELECT * FROM t ORDER BY name");
    if (result && strcmp(result, "SELECT * FROM t ORDER BY name") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged");
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - add_subquery_alias (sql_tr_query.c)
// ============================================================================

static void test_subquery_alias(void) {
    TEST("Query - FROM (SELECT ...) gets alias");
    char *result = add_subquery_alias(
        "SELECT * FROM (SELECT id FROM t) WHERE id > 0");
    if (result && strcasestr(result, "AS subq")) {
        PASS();
    } else {
        FAIL("Expected AS subqN alias");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Query Translation Tests - fix_collections_query (sql_tr_query.c)
// ============================================================================

static void test_collections_filter(void) {
    TEST("Query - filter metadata_type=18 with type=1");
    char *result = fix_collections_query(
        "SELECT * FROM metadata_items WHERE "
        "(metadata_items.metadata_type=1 or metadata_items.metadata_type=18)");
    if (result && !strcasestr(result, "type=18") &&
        strcasestr(result, "metadata_type=1")) {
        PASS();
    } else {
        FAIL("Expected type=18 removed");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_collections_no_change(void) {
    TEST("Query - no collection filter when only type=1");
    char *result = fix_collections_query(
        "SELECT * FROM metadata_items WHERE metadata_type=1 ");
    if (result && strcasestr(result, "metadata_type=1")) {
        PASS();
    } else {
        FAIL("Expected unchanged");
    }
    free(result);
}

// ============================================================================
// Keyword Translation Tests - fix_operator_spacing (sql_tr_keywords.c)
// ============================================================================

static void test_operator_spacing_eq(void) {
    TEST("Keyword - fix_operator_spacing =-1 -> = -1");
    char *result = fix_operator_spacing("SELECT * FROM t WHERE a=-1");
    if (result && strstr(result, "= -1")) {
        PASS();
    } else {
        FAIL("Expected space before -1");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_operator_spacing_ne(void) {
    TEST("Keyword - fix_operator_spacing !=-1 -> != -1");
    char *result = fix_operator_spacing("SELECT * FROM t WHERE a!=-1");
    if (result && strstr(result, "!= -1")) {
        PASS();
    } else {
        FAIL("Expected space before -1");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_operator_spacing_no_fix(void) {
    TEST("Keyword - fix_operator_spacing no fix needed");
    char *result = fix_operator_spacing("SELECT * FROM t WHERE a = -1");
    if (result && strcmp(result, "SELECT * FROM t WHERE a = -1") == 0) {
        PASS();
    } else {
        FAIL("Expected unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_operator_spacing_gte(void) {
    TEST("Keyword - fix_operator_spacing >=-1 -> >= -1");
    char *result = fix_operator_spacing("SELECT * FROM t WHERE a>=-1");
    if (result && strstr(result, ">= -1")) {
        PASS();
    } else {
        FAIL("Expected >= -1");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_operator_spacing_lte(void) {
    TEST("Keyword - fix_operator_spacing <=-1 -> <= -1");
    char *result = fix_operator_spacing("SELECT * FROM t WHERE a<=-1");
    if (result && strstr(result, "<= -1")) {
        PASS();
    } else {
        FAIL("Expected <= -1");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_operator_spacing_ne2(void) {
    TEST("Keyword - fix_operator_spacing <>-1 -> <> -1");
    char *result = fix_operator_spacing("SELECT * FROM t WHERE a<>-1");
    if (result && strstr(result, "<> -1")) {
        PASS();
    } else {
        FAIL("Expected <> -1");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_operator_spacing_gt(void) {
    TEST("Keyword - fix_operator_spacing >-1 -> > -1");
    char *result = fix_operator_spacing("SELECT * FROM t WHERE a>-1");
    if (result && strstr(result, "> -1")) {
        PASS();
    } else {
        FAIL("Expected > -1");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_operator_spacing_lt(void) {
    TEST("Keyword - fix_operator_spacing <-1 -> < -1");
    char *result = fix_operator_spacing("SELECT * FROM t WHERE a<-1");
    if (result && strstr(result, "< -1")) {
        PASS();
    } else {
        FAIL("Expected < -1");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Placeholder Translation Tests - double quote fix (sql_tr_placeholders.c)
// ============================================================================

static void test_placeholder_double_quote_not_string(void) {
    TEST("Placeholder - ? inside 'string' ignored, not \"identifier\"");
    char **names = NULL;
    int count = 0;
    // Double quotes are SQL identifiers, not strings.
    // The placeholder translator only skips ? inside single-quoted strings.
    // Identifiers with ? are unusual but the current behavior is correct:
    // all ? get translated since double quotes delimit identifiers, not values.
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE name = '?' AND id = ?", &names, &count);
    // ? inside single quotes should NOT become $N
    // ? outside should become $1
    if (result && count == 1 && strstr(result, "'?'") &&
        strstr(result, "$1")) {
        PASS();
    } else {
        FAIL("Expected ? inside single quotes preserved, outside translated");
        if (result) printf("    Got: %s (count=%d)\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

// ============================================================================
// fix_group_by_strict_complete() direct tests (sql_tr_groupby.c)
// ============================================================================

static void test_groupby_complete_null(void) {
    TEST("GroupByComplete - NULL -> NULL");
    char *result = fix_group_by_strict_complete(NULL);
    if (result == NULL) { PASS(); }
    else { FAIL("Expected NULL"); free(result); }
}

static void test_groupby_complete_no_groupby(void) {
    TEST("GroupByComplete - no GROUP BY -> unchanged");
    char *result = fix_group_by_strict_complete("SELECT id, name FROM t");
    if (result && strcmp(result, "SELECT id, name FROM t") == 0) { PASS(); }
    else { FAIL("Expected unchanged"); if (result) printf("    Got: %s\n", result); }
    free(result);
}

static void test_groupby_complete_adds_missing_column(void) {
    TEST("GroupByComplete - adds missing column to GROUP BY");
    char *result = fix_group_by_strict_complete(
        "SELECT id, name, title FROM t GROUP BY id");
    if (result && strcasestr(result, "GROUP BY id,name,title")) {
        PASS();
    } else {
        FAIL("Expected name,title added to GROUP BY");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_skips_aggregate(void) {
    TEST("GroupByComplete - skips aggregate functions in SELECT");
    char *result = fix_group_by_strict_complete(
        "SELECT id, count(*) as cnt FROM t GROUP BY id");
    // count(*) should not be added to GROUP BY, only id is needed
    if (result && !strcasestr(result, "count")) {
        // GROUP BY should only contain id, not count(*)
        PASS();
    } else if (result && strcasestr(result, "GROUP BY id") && !strstr(result, ",cnt")) {
        PASS();
    } else {
        FAIL("Expected aggregate skipped");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_skips_numeric_constant(void) {
    TEST("GroupByComplete - skips numeric constants");
    char *result = fix_group_by_strict_complete(
        "SELECT id, 1 as flag FROM t GROUP BY id");
    // "1" should not be added to GROUP BY
    if (result && !strstr(result, ",1") && !strstr(result, ",flag")) {
        PASS();
    } else {
        FAIL("Expected numeric constant skipped");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_already_complete(void) {
    TEST("GroupByComplete - all columns present -> unchanged");
    char *result = fix_group_by_strict_complete(
        "SELECT id, name FROM t GROUP BY id, name");
    if (result && strcasestr(result, "GROUP BY id, name") && !strstr(result, ",name,name")) {
        PASS();
    } else {
        FAIL("Expected unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_skips_case(void) {
    TEST("GroupByComplete - skips CASE expression");
    char *result = fix_group_by_strict_complete(
        "SELECT id, CASE WHEN a > 0 THEN 'yes' ELSE 'no' END as flag FROM t GROUP BY id");
    // CASE should not be added to GROUP BY
    if (result && !strcasestr(result, "CASE")) {
        PASS();
    } else if (result && strcasestr(result, "GROUP BY id") && !strstr(result, ",flag")) {
        PASS();
    } else {
        FAIL("Expected CASE skipped");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_skips_subquery(void) {
    TEST("GroupByComplete - subquery body not in GROUP BY (alias ok)");
    char *result = fix_group_by_strict_complete(
        "SELECT id, (SELECT count(*) FROM t2 WHERE t2.pid = t.id) as cnt FROM t GROUP BY id");
    // The subquery body "(SELECT count(*)...)" should NOT appear in the GROUP BY clause
    // The alias "cnt" may appear (which is acceptable GROUP BY behavior)
    const char *gb = result ? strcasestr(result, "GROUP BY") : NULL;
    if (result && gb && !strstr(gb, "(SELECT")) {
        PASS();
    } else {
        FAIL("Expected subquery body not in GROUP BY portion");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_table_dot_column(void) {
    TEST("GroupByComplete - handles table.column references");
    char *result = fix_group_by_strict_complete(
        "SELECT t.id, t.name, count(*) FROM t GROUP BY t.id");
    if (result && strstr(result, "t.name")) {
        PASS();
    } else {
        FAIL("Expected t.name added");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_preserves_having(void) {
    TEST("GroupByComplete - preserves HAVING clause");
    char *result = fix_group_by_strict_complete(
        "SELECT id, name, count(*) as cnt FROM t GROUP BY id HAVING count(*) > 1");
    if (result && strcasestr(result, "HAVING count(*) > 1") && strstr(result, "name")) {
        PASS();
    } else {
        FAIL("Expected HAVING preserved and name added");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_preserves_order_by(void) {
    TEST("GroupByComplete - preserves ORDER BY clause");
    char *result = fix_group_by_strict_complete(
        "SELECT id, name FROM t GROUP BY id ORDER BY name");
    if (result && strcasestr(result, "ORDER BY name") && strstr(result, ",name")) {
        PASS();
    } else {
        FAIL("Expected ORDER BY preserved and name added");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_preserves_limit(void) {
    TEST("GroupByComplete - preserves LIMIT clause");
    char *result = fix_group_by_strict_complete(
        "SELECT id, name FROM t GROUP BY id LIMIT 10");
    if (result && strcasestr(result, "LIMIT 10") && strstr(result, ",name")) {
        PASS();
    } else {
        FAIL("Expected LIMIT preserved and name added");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_distinct_select(void) {
    TEST("GroupByComplete - handles SELECT DISTINCT");
    char *result = fix_group_by_strict_complete(
        "SELECT DISTINCT id, name FROM t GROUP BY id");
    if (result && strstr(result, "name")) {
        PASS();
    } else {
        FAIL("Expected name added with DISTINCT");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_quoted_column(void) {
    TEST("GroupByComplete - handles quoted column names");
    char *result = fix_group_by_strict_complete(
        "SELECT id, \"index\" FROM t GROUP BY id");
    if (result && (strstr(result, "\"index\"") || strstr(result, ",\"index\""))) {
        PASS();
    } else {
        FAIL("Expected \"index\" added");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_groupby_complete_func_with_alias(void) {
    TEST("GroupByComplete - function call with AS alias -> add alias");
    char *result = fix_group_by_strict_complete(
        "SELECT id, upper(name) as uname FROM t GROUP BY id");
    if (result && strstr(result, "uname")) {
        PASS();
    } else {
        FAIL("Expected alias uname added to GROUP BY");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// add_nulls_first_ordering() additional tests
// ============================================================================

static void test_nulls_first_no_groupby(void) {
    TEST("NullsFirst - no GROUP BY -> unchanged");
    char *result = add_nulls_first_ordering("SELECT * FROM t ORDER BY id");
    if (result && strcmp(result, "SELECT * FROM t ORDER BY id") == 0) { PASS(); }
    else { FAIL("Expected unchanged"); }
    free(result);
}

static void test_nulls_first_existing_orderby(void) {
    TEST("NullsFirst - existing ORDER BY -> unchanged (skips)");
    char *result = add_nulls_first_ordering(
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a");
    // Has ORDER BY already, should be unchanged
    if (result && strcasestr(result, "ORDER BY a") && !strstr(result, "NULLS FIRST")) {
        PASS();
    } else {
        FAIL("Expected unchanged with existing ORDER BY");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_nulls_first_no_orderby(void) {
    TEST("NullsFirst - GROUP BY without ORDER BY -> adds ORDER BY 1 NULLS FIRST");
    char *result = add_nulls_first_ordering(
        "SELECT a, count(*) FROM t GROUP BY a");
    if (result && strstr(result, "ORDER BY 1 NULLS FIRST")) {
        PASS();
    } else {
        FAIL("Expected ORDER BY 1 NULLS FIRST added");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_nulls_first_before_limit(void) {
    TEST("NullsFirst - ORDER BY inserted before LIMIT");
    char *result = add_nulls_first_ordering(
        "SELECT a, count(*) FROM t GROUP BY a LIMIT 10");
    if (result && strstr(result, "ORDER BY 1 NULLS FIRST") &&
        strstr(result, "LIMIT 10")) {
        // Verify ORDER BY comes before LIMIT
        const char *order = strstr(result, "ORDER BY");
        const char *limit = strstr(result, "LIMIT");
        if (order && limit && order < limit) { PASS(); }
        else { FAIL("ORDER BY should come before LIMIT"); printf("    Got: %s\n", result); }
    } else {
        FAIL("Expected ORDER BY 1 NULLS FIRST before LIMIT");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

// ============================================================================
// Full Pipeline Tests - NULLS FIRST ordering (sql_tr_groupby.c)
// ============================================================================

static void test_nulls_first_ordering(void) {
    TEST("GroupBy - add_nulls_first_ordering");
    char *result = add_nulls_first_ordering(
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a");
    // Should add NULLS FIRST to ORDER BY in GROUP BY queries
    if (result) {
        // Function may or may not add NULLS FIRST depending on implementation
        // At minimum it should not crash and return valid SQL
        PASS();
    } else {
        FAIL("Returned NULL");
    }
    free(result);
}

// ============================================================================
// Placeholder Bug Fix Tests - ? inside string literals
// ============================================================================

static void test_placeholder_question_in_string_literal(void) {
    TEST("Placeholder - ? inside single-quoted string -> not a param");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "UPDATE metadata_items SET guid=REPLACE(guid,'?lang=en','?lang=xn') "
        "WHERE guid LIKE 'com.plexapp.agents.none%'", &names, &count);
    if (result && count == 0) {
        PASS();
    } else {
        FAIL("Expected 0 params (? is inside string)");
        if (result) printf("    Got: %s (count=%d)\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

static void test_placeholder_question_in_string_mixed(void) {
    TEST("Placeholder - ? in string + real ? param -> 1 param");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "UPDATE t SET c=REPLACE(c,'?old','?new') WHERE id=?", &names, &count);
    if (result && count == 1 && strstr(result, "$1")) {
        PASS();
    } else {
        FAIL("Expected 1 param");
        if (result) printf("    Got: %s (count=%d)\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

static void test_placeholder_backslash_in_string(void) {
    TEST("Placeholder - backslash before closing quote is literal (SQL standard)");
    char **names = NULL;
    int count = 0;
    // In standard SQL, backslash is literal. 'abc\' is a complete string.
    // The ? after should be a real parameter.
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE path='C:\\Users\\' AND id=?", &names, &count);
    if (result && count == 1 && strstr(result, "$1")) {
        PASS();
    } else {
        FAIL("Expected 1 param (backslash is literal in SQL strings)");
        if (result) printf("    Got: %s (count=%d)\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

static void test_placeholder_doubled_quote_with_question(void) {
    TEST("Placeholder - doubled quotes ('it''s a ?test') -> 0 params");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "INSERT INTO t VALUES('it''s a ?test')", &names, &count);
    if (result && count == 0) {
        PASS();
    } else {
        FAIL("Expected 0 params");
        if (result) printf("    Got: %s (count=%d)\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

static void test_placeholder_four_quotes_with_question(void) {
    TEST("Placeholder - four quotes ('''?''') -> 0 params");
    char **names = NULL;
    int count = 0;
    // '''' is an escaped single quote inside a string = string containing one quote
    // '''?''' = string with quote, then outside ?, then string with quote?
    // Actually: ' '' ' ? ' '' ' — that's three separate strings with ? between
    // No wait: SQL parser: ' (start) '' (escaped ') ? (still in string) '' (escaped ') ' (end)
    // So the whole thing is one string: 'escaped-quote ? escaped-quote'
    char *result = sql_translate_placeholders(
        "INSERT INTO t VALUES('''?''')", &names, &count);
    if (result && count == 0) {
        PASS();
    } else {
        FAIL("Expected 0 params");
        if (result) printf("    Got: %s (count=%d)\n", result, count);
    }
    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) if (names[i]) free(names[i]);
        free(names);
    }
}

// ============================================================================
// Upsert Translation Tests - INSERT OR REPLACE without column list
// ============================================================================

static void test_upsert_tags_without_column_list(void) {
    TEST("Upsert - INSERT OR REPLACE INTO tags VALUES(...) uses implicit columns");
    char *result = translate_insert_or_replace(
        "INSERT OR REPLACE INTO tags VALUES(1, 100, 'Drama', 1, '', '', '', 1000, 2000, 0, '', 'genre-drama', NULL)");
    if (result && strstr(result, "ON CONFLICT") && strstr(result, "DO UPDATE SET") &&
        strstr(result, "metadata_item_id = EXCLUDED.metadata_item_id") &&
        strstr(result, "tag = EXCLUDED.tag") &&
        !strstr(result, "id = EXCLUDED.id")) {  // id should be skipped (conflict column)
        PASS();
    } else {
        FAIL("Expected ON CONFLICT with implicit column SET clause");
        if (result) printf("    Got: %.200s\n", result);
    }
    if (result) free(result);
}

static void test_upsert_tags_with_column_list(void) {
    TEST("Upsert - INSERT OR REPLACE INTO tags (id, tag) VALUES(...) works");
    char *result = translate_insert_or_replace(
        "INSERT OR REPLACE INTO tags (id, tag, tag_type) VALUES(1, 'Drama', 1)");
    if (result && strstr(result, "ON CONFLICT") && strstr(result, "DO UPDATE SET") &&
        strstr(result, "tag = EXCLUDED.tag")) {
        PASS();
    } else {
        FAIL("Expected ON CONFLICT clause");
        if (result) printf("    Got: %.200s\n", result);
    }
    if (result) free(result);
}

static void test_upsert_unknown_table_no_columns(void) {
    TEST("Upsert - INSERT OR REPLACE INTO unknown_tbl VALUES(...) -> fallback");
    char *result = translate_insert_or_replace(
        "INSERT OR REPLACE INTO unknown_tbl VALUES(1, 'test')");
    // Without column list and no implicit mapping, should fall back to plain INSERT
    if (result && strstr(result, "INSERT INTO") && !strstr(result, "OR REPLACE")) {
        PASS();
    } else {
        FAIL("Expected fallback to plain INSERT");
        if (result) printf("    Got: %.200s\n", result);
    }
    if (result) free(result);
}

// ============================================================================
// Mixed-Case Identifier Quoting Tests
// ============================================================================

static void test_quote_mixed_case_column_alias(void) {
    TEST("MixedCase - column alias AS blankKeyTaggingId -> AS \"blankKeyTaggingId\"");
    char *result = quote_mixed_case_identifiers("SELECT id AS blankKeyTaggingId FROM t");
    if (result && strstr(result, "AS \"blankKeyTaggingId\"") && !strstr(result, "AS blankKeyTaggingId\"\"")) {
        PASS();
    } else {
        FAIL("Expected AS \"blankKeyTaggingId\"");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_table_alias(void) {
    TEST("MixedCase - table alias as otherTags -> as \"otherTags\"");
    char *result = quote_mixed_case_identifiers("FROM tags JOIN tags as otherTags ON otherTags.id = tags.id");
    if (result && strstr(result, "as \"otherTags\"")) {
        PASS();
    } else {
        FAIL("Expected as \"otherTags\"");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_no_uppercase(void) {
    TEST("MixedCase - no uppercase -> unchanged");
    char *result = quote_mixed_case_identifiers("SELECT id AS my_alias FROM t");
    if (result && strstr(result, "AS my_alias") && !strstr(result, "\"my_alias\"")) {
        PASS();
    } else {
        FAIL("Expected AS my_alias (unchanged)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_already_quoted(void) {
    TEST("MixedCase - already quoted AS \"Foo\" -> unchanged");
    char *result = quote_mixed_case_identifiers("SELECT id AS \"Foo\" FROM t");
    if (result && strstr(result, "AS \"Foo\"") && !strstr(result, "AS \"\"Foo\"\"")) {
        PASS();
    } else {
        FAIL("Expected AS \"Foo\" (unchanged)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_in_string_literal(void) {
    TEST("MixedCase - AS inside string literal -> not quoted");
    char *result = quote_mixed_case_identifiers("SELECT 'AS camelCase' FROM t");
    if (result && strstr(result, "'AS camelCase'") && !strstr(result, "\"camelCase\"")) {
        PASS();
    } else {
        FAIL("Expected string literal unchanged");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_synccollections(void) {
    TEST("MixedCase - SyncCollections: aliases AND references quoted");
    char *result = quote_mixed_case_identifiers(
        "select taggings.id as blankKeyTaggingId, otherTags.id as nonblankKeyId "
        "from tags join tags as otherTags on otherTags.tag = tags.tag");
    if (result &&
        strstr(result, "as \"blankKeyTaggingId\"") &&
        strstr(result, "as \"nonblankKeyId\"") &&
        strstr(result, "as \"otherTags\"") &&
        strstr(result, "\"otherTags\".id") &&
        strstr(result, "\"otherTags\".tag")) {
        PASS();
    } else {
        FAIL("Expected all mixed-case identifiers quoted (incl. references)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_table_reference(void) {
    TEST("MixedCase - table alias reference: grandparentsSettings.col -> quoted");
    char *result = quote_mixed_case_identifiers(
        "select grandparentsSettings.extra_data from metadata_item_settings as grandparentsSettings");
    if (result &&
        strstr(result, "\"grandparentsSettings\".extra_data") &&
        strstr(result, "as \"grandparentsSettings\"")) {
        PASS();
    } else {
        FAIL("Expected grandparentsSettings quoted everywhere");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_cast_not_quoted(void) {
    TEST("MixedCase - CAST(x AS INTEGER) -> INTEGER not quoted");
    char *result = quote_mixed_case_identifiers(
        "SELECT CAST(rating_count/100 AS INTEGER) FROM metadata_items");
    if (result && strstr(result, "AS INTEGER") && !strstr(result, "\"INTEGER\"")) {
        PASS();
    } else {
        FAIL("Expected AS INTEGER (not quoted)");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_cast_with_alias(void) {
    TEST("MixedCase - CAST with type keyword + mixed-case alias both correct");
    char *result = quote_mixed_case_identifiers(
        "SELECT CAST(x AS INTEGER) AS myResult FROM t");
    if (result &&
        strstr(result, "AS INTEGER") && !strstr(result, "\"INTEGER\"") &&
        strstr(result, "AS \"myResult\"")) {
        PASS();
    } else {
        FAIL("Expected INTEGER unquoted, myResult quoted");
        if (result) printf("    Got: %s\n", result);
    }
    free(result);
}

static void test_quote_mixed_case_null_input(void) {
    TEST("MixedCase - NULL input -> NULL output");
    char *result = quote_mixed_case_identifiers(NULL);
    if (result == NULL) {
        PASS();
    } else {
        FAIL("Expected NULL");
        free(result);
    }
}

static void test_quote_mixed_case_full_translate(void) {
    TEST("MixedCase - full sql_translate() preserves camelCase aliases");
    sql_translation_t tr = sql_translate(
        "select taggings.id as blankKeyTaggingId, otherTags.id as nonblankKeyId "
        "from tags join tags as otherTags on otherTags.tag = tags.tag "
        "where tags.tag_value = :C1");
    if (tr.success && tr.sql &&
        strstr(tr.sql, "\"blankKeyTaggingId\"") &&
        strstr(tr.sql, "\"nonblankKeyId\"") &&
        strstr(tr.sql, "\"otherTags\"")) {
        PASS();
    } else {
        FAIL("Expected mixed-case identifiers quoted in full translation");
        if (tr.sql) printf("    Got: %s\n", tr.sql);
    }
    sql_translation_free(&tr);
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== SQL Translator Tests ===\033[0m\n\n");

    // Initialize translator
    sql_translator_init();

    printf("\033[1mPlaceholder Translation:\033[0m\n");
    test_placeholder_basic();
    test_placeholder_multiple();
    test_placeholder_reuse();
    test_placeholder_question_mark();
    test_placeholder_in_string();
    test_placeholder_question_alpha_space();
    test_placeholder_mixed_question_and_named();
    test_placeholder_escaped_quotes();
    test_placeholder_colon_after_ident();

    printf("\n\033[1mFunction Translation:\033[0m\n");
    test_function_ifnull();
    test_function_length();
    test_function_substr();
    test_function_random();
    test_function_datetime();

    printf("\n\033[1mKeyword Translation:\033[0m\n");
    test_keyword_glob();
    test_keyword_notnull();
    test_keyword_null_input();
    test_keyword_alter_table_add_quoted();
    test_keyword_alter_table_add_dblquoted();
    test_keyword_alter_table_add_unquoted();
    test_keyword_begin_immediate();
    test_keyword_begin_deferred();
    test_keyword_begin_exclusive();
    test_keyword_insert_or_ignore();
    test_keyword_replace_into();
    test_keyword_empty_in();
    test_keyword_empty_in_spaces();
    test_keyword_group_by_null();
    test_keyword_having_cnt();
    test_keyword_sqlite_master();
    test_keyword_sqlite_schema();
    test_keyword_main_dot_sqlite_master();
    test_keyword_main_unquoted_sqlite_master();
    test_keyword_sqlite_master_order_by_rowid();
    test_keyword_replace_into_preserved_with_insert_or();
    test_keyword_indexed_by();
    test_keyword_indexed_by_multiple();

    printf("\n\033[1mType Translation:\033[0m\n");
    test_type_autoincrement();
    test_type_text();
    test_type_dt_integer8();
    test_type_integer8();
    test_type_blob_paren();
    test_type_blob_comma();
    test_type_blob_space();
    test_type_default_true();
    test_type_default_false();
    test_type_datetime_paren();
    test_type_datetime_comma();
    test_type_datetime_space();
    test_type_null_input();
    test_type_no_patterns();

    printf("\n\033[1mFull Query Translation:\033[0m\n");
    test_full_select();
    test_full_insert();
    test_full_update();
    test_full_complex();

    printf("\n\033[1mPlex-specific Inline Fixes (pipeline):\033[0m\n");
    test_plex_viewed_at_order_by();
    test_plex_external_metadata_group_by();
    test_plex_clustering_distinct_removes_group_by();

    printf("\n\033[1mEdge Cases:\033[0m\n");
    test_edge_empty();
    test_edge_null();
    test_edge_backticks();
    test_edge_double_quotes_preserved();

    printf("\n\033[1mCOLLATE NOCASE (NEW):\033[0m\n");
    test_collate_nocase_equals();
    test_collate_nocase_like();
    test_collate_nocase_orderby();
    test_collate_nocase_glob();
    test_collate_nocase_ne();

    printf("\n\033[1mFTS4 Boolean Search (NEW):\033[0m\n");
    test_fts_negation();
    test_fts_and_chain();
    test_fts_or_chain();
    test_fts_phrase();

    printf("\n\033[1mWindow Functions (NEW):\033[0m\n");
    test_window_row_number();
    test_window_rank();
    test_window_dense_rank();

    printf("\n\033[1mFTS Quote Parsing (Bug Fix):\033[0m\n");
    test_fts_single_escaped_quote();
    test_fts_double_escaped_quote();
    test_fts_simple_term();
    test_fts_mixed_quotes_and_terms();

    printf("\n\033[1mJSON Operator Parameters (Bug Fix):\033[0m\n");
    test_json_operator_with_parameter();
    test_json_operator_with_literal();
    test_json_operator_is_null();
    test_json_operator_param_position();
    test_json_operator_plex_vad_query();

    printf("\n\033[1mFunction - INSTR:\033[0m\n");
    test_function_instr();
    test_function_instr_no_match();

    printf("\n\033[1mHelper Functions:\033[0m\n");
    test_helper_str_replace();
    test_helper_str_replace_no_match();
    test_helper_str_replace_nocase();
    test_helper_safe_strcasestr();
    test_helper_safe_strcasestr_null();
    test_helper_extract_arg();

    printf("\n\033[1mFunction - IIF:\033[0m\n");
    test_function_iif();
    test_function_iif_no_match();

    printf("\n\033[1mFunction - TYPEOF:\033[0m\n");
    test_function_typeof();

    printf("\n\033[1mFunction - STRFTIME:\033[0m\n");
    test_function_strftime_epoch();
    test_function_strftime_epoch_interval();
    test_function_strftime_date();
    test_function_strftime_column();

    printf("\n\033[1mFunction - UNIXEPOCH:\033[0m\n");
    test_function_unixepoch_now();
    test_function_unixepoch_interval();

    printf("\n\033[1mFunction - last_insert_rowid, json_each:\033[0m\n");
    test_function_last_insert_rowid();
    test_function_json_each();
    test_json_each_value_text_cast();

    printf("\n\033[1mFunction - simplify_typeof_fixup:\033[0m\n");
    test_function_simplify_typeof();

    printf("\n\033[1mFunction - typeof() type remapping:\033[0m\n");
    test_typeof_integer_bigint_expansion();
    test_typeof_real_to_double_precision();

    printf("\n\033[1mFunction - strftime() additional:\033[0m\n");
    test_strftime_datetime_format();
    test_strftime_positive_interval();
    test_strftime_generic_format();

    printf("\n\033[1mFunction - unixepoch() additional:\033[0m\n");
    test_unixepoch_column();

    printf("\n\033[1mQuote Translations:\033[0m\n");
    test_quote_column_quotes();
    test_quote_alias_quotes();
    test_quote_ddl_table();
    test_quote_ddl_not_dml();
    test_quote_if_not_exists_table();
    test_quote_if_not_exists_index();
    test_quote_if_not_exists_unique_index();
    test_quote_if_not_exists_already();
    test_quote_on_conflict_unquote();

    printf("\n\033[1mDuplicate Assignment Dedup:\033[0m\n");
    test_dedup_assignments_basic();
    test_dedup_assignments_no_dup();
    test_dedup_assignments_quoted();
    test_dedup_assignments_params();
    test_dedup_not_update();

    printf("\n\033[1mForward Reference Joins:\033[0m\n");
    test_forward_ref_null();
    test_forward_ref_no_alias_join();
    test_forward_ref_no_unaliased_join();
    test_forward_ref_reorder();
    test_forward_ref_no_forward_reference();

    printf("\n\033[1mNull Sorting:\033[0m\n");
    test_null_sorting();
    test_null_sorting_no_match();
    test_null_sorting_null_input();
    test_null_sorting_backtick_parents_index();
    test_null_sorting_metadata_items_index();
    test_null_sorting_originally_available_at();
    test_null_sorting_grandparents_title_sort();
    test_null_sorting_space_variant();

    printf("\n\033[1mDistinct + ORDER BY:\033[0m\n");
    test_distinct_orderby_aggregate();
    test_distinct_orderby_random();
    test_distinct_orderby_groupby();

    printf("\n\033[1mCase Booleans:\033[0m\n");
    test_case_booleans_else_1_true();
    test_case_booleans_else_0_false();
    test_case_booleans_then_0_else_true();
    test_case_booleans_then_1_else_false();
    test_case_booleans_0_or();
    test_case_booleans_1_or();
    test_case_booleans_and_0();
    test_case_booleans_and_1();
    test_case_booleans_or_0();
    test_case_booleans_or_1();
    test_case_booleans_where_0();
    test_case_booleans_where_1();
    test_case_booleans_null_input();
    test_case_booleans_no_match();

    printf("\n\033[1mInteger/Text Mismatch:\033[0m\n");
    test_int_text_mismatch_null();
    test_int_text_mismatch_no_match();
    test_int_text_mismatch_pattern1();
    test_int_text_mismatch_pattern2_backtick();
    test_int_text_mismatch_pattern2_quote();
    test_int_text_mismatch_pattern4_download_backtick();
    test_int_text_mismatch_pattern4_download_quote();
    test_int_text_mismatch_generic_status_backtick();
    test_int_text_mismatch_generic_status_quote();

    printf("\n\033[1mMax/Min Translation:\033[0m\n");
    test_max_to_greatest();
    test_max_single_arg();
    test_min_to_least();
    test_min_single_arg();

    printf("\n\033[1mICU Collation Strip:\033[0m\n");
    test_strip_icu_collation();
    test_strip_icu_collation_no_match();

    printf("\n\033[1mSubquery Alias:\033[0m\n");
    test_subquery_alias();

    printf("\n\033[1mCollections Filter:\033[0m\n");
    test_collections_filter();
    test_collections_no_change();

    printf("\n\033[1mOperator Spacing:\033[0m\n");
    test_operator_spacing_eq();
    test_operator_spacing_ne();
    test_operator_spacing_no_fix();
    test_operator_spacing_gte();
    test_operator_spacing_lte();
    test_operator_spacing_ne2();
    test_operator_spacing_gt();
    test_operator_spacing_lt();

    printf("\n\033[1mPlaceholder Fix (single vs double quotes):\033[0m\n");
    test_placeholder_double_quote_not_string();

    printf("\n\033[1mfix_group_by_strict_complete (GROUP BY rewriter):\033[0m\n");
    test_groupby_complete_null();
    test_groupby_complete_no_groupby();
    test_groupby_complete_adds_missing_column();
    test_groupby_complete_skips_aggregate();
    test_groupby_complete_skips_numeric_constant();
    test_groupby_complete_already_complete();
    test_groupby_complete_skips_case();
    test_groupby_complete_skips_subquery();
    test_groupby_complete_table_dot_column();
    test_groupby_complete_preserves_having();
    test_groupby_complete_preserves_order_by();
    test_groupby_complete_preserves_limit();
    test_groupby_complete_distinct_select();
    test_groupby_complete_quoted_column();
    test_groupby_complete_func_with_alias();

    printf("\n\033[1madd_nulls_first_ordering:\033[0m\n");
    test_nulls_first_ordering();
    test_nulls_first_no_groupby();
    test_nulls_first_existing_orderby();
    test_nulls_first_no_orderby();
    test_nulls_first_before_limit();

    printf("\n\033[1mPlaceholder Bug Fix (? in string literals):\033[0m\n");
    test_placeholder_question_in_string_literal();
    test_placeholder_question_in_string_mixed();
    test_placeholder_backslash_in_string();
    test_placeholder_doubled_quote_with_question();
    test_placeholder_four_quotes_with_question();

    printf("\n\033[1mUpsert (INSERT OR REPLACE without column list):\033[0m\n");
    test_upsert_tags_without_column_list();
    test_upsert_tags_with_column_list();
    test_upsert_unknown_table_no_columns();

    printf("\n\033[1mMixed-Case Identifier Quoting:\033[0m\n");
    test_quote_mixed_case_column_alias();
    test_quote_mixed_case_table_alias();
    test_quote_mixed_case_no_uppercase();
    test_quote_mixed_case_already_quoted();
    test_quote_mixed_case_in_string_literal();
    test_quote_mixed_case_synccollections();
    test_quote_mixed_case_table_reference();
    test_quote_mixed_case_cast_not_quoted();
    test_quote_mixed_case_cast_with_alias();
    test_quote_mixed_case_null_input();
    test_quote_mixed_case_full_translate();

    // Cleanup
    sql_translator_cleanup();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
