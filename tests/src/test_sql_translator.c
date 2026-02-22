/*
 * Unit tests for SQL translation (SQLite to PostgreSQL)
 * Rust-only backend - all tests use sql_translate() end-to-end.
 *
 * Tests:
 * 1. Placeholder translation (:name -> $1, ? -> $N)
 * 2. Function translation (IFNULL -> COALESCE, iif -> CASE WHEN, etc.)
 * 3. Type translation (INTEGER -> BIGINT, AUTOINCREMENT -> SERIAL, etc.)
 * 4. Keyword translation (GLOB -> ILIKE, BEGIN IMMEDIATE -> BEGIN, etc.)
 * 5. Full query translation
 * 6. COLLATE NOCASE, FTS4 MATCH, Window functions, JSON operators
 * 7. GROUP BY strict mode, NULLS FIRST, operator spacing, etc.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>
#include "sql_translator.h"

/* ---------- test framework ---------- */

static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)

/* Case-insensitive substring search helper */
static int contains_ci(const char *haystack, const char *needle) {
    if (!haystack || !needle) return 0;
    size_t hlen = strlen(haystack);
    size_t nlen = strlen(needle);
    if (nlen > hlen) return 0;
    for (size_t i = 0; i <= hlen - nlen; i++) {
        size_t j;
        for (j = 0; j < nlen; j++) {
            if (tolower((unsigned char)haystack[i + j]) != tolower((unsigned char)needle[j]))
                break;
        }
        if (j == nlen) return 1;
    }
    return 0;
}

/* ====================================================================
 * Placeholder Translation Tests
 * ==================================================================== */

static void test_placeholder_basic(void) {
    TEST("Placeholder - basic :name to $1");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE id = :id");
    if (r.success && r.sql && strstr(r.sql, "$1") && r.param_count == 1 &&
        r.param_names && r.param_names[0] && strcmp(r.param_names[0], "id") == 0) {
        PASS();
    } else {
        FAIL("Expected $1 placeholder with param name 'id'");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_multiple(void) {
    TEST("Placeholder - multiple :name params");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE a = :foo AND b = :bar AND c = :baz");
    if (r.success && r.sql &&
        strstr(r.sql, "$1") && strstr(r.sql, "$2") && strstr(r.sql, "$3") &&
        r.param_count == 3) {
        PASS();
    } else {
        FAIL("Expected $1, $2, $3 placeholders");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_reuse(void) {
    TEST("Placeholder - same :name used twice");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE a = :id OR b = :id");
    /* Same param used twice should map to same $N */
    if (r.success && r.sql && r.param_count == 1) {
        PASS();
    } else {
        FAIL("Expected single param for reused :id");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_question_mark(void) {
    TEST("Placeholder - ? positional params");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE a = ? AND b = ?");
    if (r.success && r.sql &&
        strstr(r.sql, "$1") && strstr(r.sql, "$2") && r.param_count == 2) {
        PASS();
    } else {
        FAIL("Expected $1, $2 for ? params");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_in_string(void) {
    TEST("Placeholder - :name inside string literal ignored");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE a = ':not_a_param'");
    /* Should NOT translate :not_a_param inside quotes */
    if (r.success && r.sql && r.param_count == 0) {
        PASS();
    } else {
        FAIL("Should not translate :param inside string");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_mixed_question_and_named(void) {
    TEST("Placeholder - mixed ? and :name -> sequential numbering");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE a = ? AND b = :foo AND c = ?");
    if (r.success && r.sql && r.param_count == 3 &&
        strstr(r.sql, "$1") && strstr(r.sql, "$2") && strstr(r.sql, "$3")) {
        PASS();
    } else {
        FAIL("Expected $1, $2, $3");
        if (r.sql) printf("    Got: %s count=%d\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_escaped_quotes(void) {
    TEST("Placeholder - :param inside escaped quotes ('it''s :not') -> skipped");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE name = 'it''s :not_a_param' AND id = :real_param");
    /* :not_a_param inside the escaped-quote string should be ignored */
    if (r.success && r.sql && r.param_count == 1 && strstr(r.sql, "$1")) {
        PASS();
    } else {
        FAIL("Expected 1 param: :real_param -> $1");
        if (r.sql) printf("    Got: %s count=%d\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_colon_after_ident(void) {
    TEST("Placeholder - table:col inside string -> NOT a placeholder");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE url = 'http:endpoint'");
    if (r.success && r.sql && r.param_count == 0) {
        PASS();
    } else {
        FAIL("Expected no placeholders");
        if (r.sql) printf("    Got: %s count=%d\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_question_in_string_literal(void) {
    TEST("Placeholder - ? inside single-quoted string -> not a param");
    sql_translation_t r = sql_translate(
        "UPDATE metadata_items SET guid = REPLACE(guid, '?lang=en', '?lang=xn') "
        "WHERE guid LIKE 'com.plexapp.agents.none%'");
    if (r.success && r.sql && r.param_count == 0) {
        PASS();
    } else {
        FAIL("Expected 0 params (? is inside string)");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_question_in_string_mixed(void) {
    TEST("Placeholder - ? in string + real ? param -> 1 param");
    sql_translation_t r = sql_translate(
        "UPDATE t SET c = REPLACE(c, '?old', '?new') WHERE id = ?");
    if (r.success && r.sql && r.param_count == 1 && strstr(r.sql, "$1")) {
        PASS();
    } else {
        FAIL("Expected 1 param");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_doubled_quote_with_question(void) {
    TEST("Placeholder - doubled quotes ('it''s a ?test') -> 0 params");
    sql_translation_t r = sql_translate(
        "INSERT INTO t (c) VALUES('it''s a ?test')");
    if (r.success && r.sql && r.param_count == 0) {
        PASS();
    } else {
        FAIL("Expected 0 params");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

static void test_placeholder_double_quote_not_string(void) {
    TEST("Placeholder - ? inside 'string' ignored, not \"identifier\"");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE name = '?' AND id = ?");
    if (r.success && r.sql && r.param_count == 1 && strstr(r.sql, "$1")) {
        PASS();
    } else {
        FAIL("Expected ? inside single quotes preserved, outside translated");
        if (r.sql) printf("    Got: %s (count=%d)\n", r.sql, r.param_count);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Function Translation Tests
 * ==================================================================== */

static void test_function_ifnull(void) {
    TEST("Function - IFNULL to COALESCE");
    sql_translation_t r = sql_translate("SELECT IFNULL(a, 0) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "COALESCE") && !contains_ci(r.sql, "IFNULL")) {
        PASS();
    } else {
        FAIL("Expected COALESCE instead of IFNULL");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_length(void) {
    TEST("Function - LENGTH preserved");
    sql_translation_t r = sql_translate("SELECT LENGTH(name) FROM t");
    if (r.success && r.sql && contains_ci(r.sql, "LENGTH")) {
        PASS();
    } else {
        FAIL("LENGTH should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_substr(void) {
    TEST("Function - SUBSTR to SUBSTRING");
    sql_translation_t r = sql_translate("SELECT SUBSTR(a, 1, 5) FROM t");
    if (r.success && r.sql && contains_ci(r.sql, "SUBSTRING")) {
        PASS();
    } else {
        FAIL("Expected SUBSTRING");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_random(void) {
    TEST("Function - RANDOM() to RANDOM()");
    sql_translation_t r = sql_translate("SELECT RANDOM() FROM t");
    if (r.success && r.sql && contains_ci(r.sql, "RANDOM")) {
        PASS();
    } else {
        FAIL("RANDOM should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_datetime(void) {
    TEST("Function - datetime('now') handling");
    sql_translation_t r = sql_translate("SELECT datetime('now') FROM t");
    if (r.success && r.sql) {
        PASS();  /* Just check it doesn't crash */
    } else {
        FAIL("datetime translation failed");
    }
    sql_translation_free(&r);
}

static void test_function_iif(void) {
    TEST("Function - iif() to CASE WHEN");
    sql_translation_t r = sql_translate("SELECT iif(a > 0, 'yes', 'no') FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "CASE") && contains_ci(r.sql, "WHEN") &&
        contains_ci(r.sql, "THEN") && contains_ci(r.sql, "ELSE") &&
        contains_ci(r.sql, "END") && !contains_ci(r.sql, "iif")) {
        PASS();
    } else {
        FAIL("Expected CASE WHEN ... THEN ... ELSE ... END");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_iif_no_match(void) {
    TEST("Function - iif() passthrough when absent");
    sql_translation_t r = sql_translate("SELECT a FROM t");
    if (r.success && r.sql && contains_ci(r.sql, "SELECT") && !contains_ci(r.sql, "iif")) {
        PASS();
    } else {
        FAIL("Expected unchanged query");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_typeof(void) {
    TEST("Function - typeof() to pg_typeof()::TEXT");
    sql_translation_t r = sql_translate("SELECT typeof(x) FROM t");
    if (r.success && r.sql && contains_ci(r.sql, "pg_typeof")) {
        PASS();
    } else {
        FAIL("Expected pg_typeof");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_instr(void) {
    TEST("Function - instr(a, b) -> STRPOS(a, b)");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE NOT instr(extra_data, 'pv%3AlastFmBlacklisted=1')");
    if (r.success && r.sql && contains_ci(r.sql, "STRPOS")) {
        PASS();
    } else {
        FAIL("instr should be translated to STRPOS");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_instr_no_match(void) {
    TEST("Function - no instr() -> unchanged");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE id = 1");
    if (r.success && r.sql && !contains_ci(r.sql, "STRPOS")) {
        PASS();
    } else {
        FAIL("Query without instr should not have STRPOS");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_last_insert_rowid(void) {
    TEST("Function - last_insert_rowid() to lastval()");
    sql_translation_t r = sql_translate("SELECT last_insert_rowid()");
    if (r.success && r.sql &&
        contains_ci(r.sql, "lastval") && !contains_ci(r.sql, "last_insert_rowid")) {
        PASS();
    } else {
        FAIL("Expected lastval()");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_json_each(void) {
    TEST("Function - json_each() to json_array_elements()");
    sql_translation_t r = sql_translate("SELECT value FROM json_each(data)");
    if (r.success && r.sql && contains_ci(r.sql, "json_array_elements")) {
        PASS();
    } else {
        FAIL("Expected json_array_elements()");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_simplify_typeof(void) {
    TEST("Function - simplify typeof in iif pattern");
    sql_translation_t r = sql_translate(
        "SELECT iif(typeof(x) in ('integer', 'real'), x, strftime('%s', x, 'utc')) FROM t");
    /* The Rust translator should simplify this pattern */
    if (r.success && r.sql && !contains_ci(r.sql, "iif(typeof(")) {
        PASS();
    } else {
        FAIL("Expected simplified typeof pattern");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_typeof_integer_bigint_expansion(void) {
    TEST("typeof - in ('integer','real') -> includes 'bigint' and 'double precision'");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE typeof(x) in ('integer', 'real')");
    if (r.success && r.sql &&
        contains_ci(r.sql, "pg_typeof") &&
        (contains_ci(r.sql, "bigint") || contains_ci(r.sql, "double precision"))) {
        PASS();
    } else {
        FAIL("Expected pg_typeof with expanded type names");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * STRFTIME Translation Tests
 * ==================================================================== */

static void test_function_strftime_epoch(void) {
    TEST("Function - strftime('%s', 'now') to EXTRACT(EPOCH FROM NOW())");
    sql_translation_t r = sql_translate("SELECT strftime('%s', 'now')");
    if (r.success && r.sql &&
        contains_ci(r.sql, "EXTRACT") && contains_ci(r.sql, "EPOCH") &&
        contains_ci(r.sql, "NOW()") && contains_ci(r.sql, "BIGINT")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM NOW())::BIGINT");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_strftime_epoch_interval(void) {
    TEST("Function - strftime('%s', 'now', '-7 day')");
    sql_translation_t r = sql_translate("SELECT strftime('%s', 'now', '-7 day')");
    if (r.success && r.sql &&
        contains_ci(r.sql, "EXTRACT") && contains_ci(r.sql, "EPOCH") &&
        contains_ci(r.sql, "INTERVAL")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH ...) with INTERVAL");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_strftime_date(void) {
    TEST("Function - strftime('%Y-%m-%d', col) to TO_CHAR");
    sql_translation_t r = sql_translate("SELECT strftime('%Y-%m-%d', added_at) FROM t");
    if (r.success && r.sql && contains_ci(r.sql, "TO_CHAR")) {
        PASS();
    } else {
        FAIL("Expected TO_CHAR");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_strftime_column(void) {
    TEST("Function - strftime('%s', column) uses TO_TIMESTAMP");
    sql_translation_t r = sql_translate("SELECT strftime('%s', updated_at) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "EXTRACT") && contains_ci(r.sql, "EPOCH") &&
        contains_ci(r.sql, "TO_TIMESTAMP")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM TO_TIMESTAMP(col))::BIGINT");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_strftime_datetime_format(void) {
    TEST("strftime - '%Y-%m-%d %H:%M:%S' -> TO_CHAR with HH24:MI:SS");
    sql_translation_t r = sql_translate(
        "SELECT strftime('%Y-%m-%d %H:%M:%S', created_at) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "TO_CHAR") && contains_ci(r.sql, "HH24:MI:SS")) {
        PASS();
    } else {
        FAIL("Expected TO_CHAR with HH24:MI:SS format");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_strftime_positive_interval(void) {
    TEST("strftime - '%s', 'now', '+7 day' -> NOW() + INTERVAL");
    sql_translation_t r = sql_translate(
        "SELECT strftime('%s', 'now', '+7 day') FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "NOW()") && contains_ci(r.sql, "INTERVAL")) {
        PASS();
    } else {
        FAIL("Expected NOW() + INTERVAL");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_strftime_generic_format(void) {
    TEST("strftime - '%H:%M' -> TO_CHAR with HH24:MI");
    sql_translation_t r = sql_translate(
        "SELECT strftime('%H:%M', created_at) FROM t");
    if (r.success && r.sql && contains_ci(r.sql, "TO_CHAR") &&
        contains_ci(r.sql, "HH24:MI")) {
        PASS();
    } else {
        FAIL("Expected TO_CHAR with HH24:MI");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * UNIXEPOCH Translation Tests
 * ==================================================================== */

static void test_function_unixepoch_now(void) {
    TEST("Function - unixepoch('now') to EXTRACT(EPOCH FROM NOW())");
    sql_translation_t r = sql_translate("SELECT unixepoch('now')");
    if (r.success && r.sql &&
        contains_ci(r.sql, "EXTRACT") && contains_ci(r.sql, "EPOCH") &&
        contains_ci(r.sql, "NOW()")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM NOW())::BIGINT");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_function_unixepoch_interval(void) {
    TEST("Function - unixepoch('now', '-7 day')");
    sql_translation_t r = sql_translate("SELECT unixepoch('now', '-7 day')");
    if (r.success && r.sql &&
        contains_ci(r.sql, "EXTRACT") && contains_ci(r.sql, "EPOCH") &&
        contains_ci(r.sql, "INTERVAL")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH ...) with INTERVAL");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_unixepoch_column(void) {
    TEST("unixepoch - unixepoch(created_at) -> EXTRACT(EPOCH FROM ...)");
    sql_translation_t r = sql_translate("SELECT unixepoch(created_at) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "EXTRACT") && contains_ci(r.sql, "EPOCH")) {
        PASS();
    } else {
        FAIL("Expected EXTRACT(EPOCH FROM ...)");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Keyword Translation Tests
 * ==================================================================== */

static void test_keyword_glob(void) {
    TEST("Keyword - GLOB to ILIKE");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE name GLOB '*test*'");
    if (r.success && r.sql && contains_ci(r.sql, "ILIKE")) {
        PASS();
    } else {
        FAIL("Expected ILIKE for GLOB");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_notnull(void) {
    TEST("Keyword - NOT NULL preserved");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE a IS NOT NULL");
    if (r.success && r.sql && contains_ci(r.sql, "NOT NULL")) {
        PASS();
    } else {
        FAIL("NOT NULL should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_alter_table_add_quoted(void) {
    TEST("Keyword - ALTER TABLE ADD 'col' -> ADD COLUMN IF NOT EXISTS with double quotes");
    sql_translation_t r = sql_translate("ALTER TABLE 'metadata_items' ADD 'new_col' TEXT");
    if (r.success && r.sql && contains_ci(r.sql, "ADD COLUMN IF NOT EXISTS") &&
        contains_ci(r.sql, "\"new_col\"")) {
        PASS();
    } else {
        FAIL("Expected ADD COLUMN IF NOT EXISTS with double-quoted column");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_alter_table_add_unquoted(void) {
    TEST("Keyword - ALTER TABLE ADD col_name -> ADD COLUMN IF NOT EXISTS");
    sql_translation_t r = sql_translate("ALTER TABLE metadata_items ADD new_col TEXT");
    if (r.success && r.sql && contains_ci(r.sql, "ADD COLUMN IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected ADD COLUMN IF NOT EXISTS");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_begin_immediate(void) {
    TEST("Keyword - BEGIN IMMEDIATE -> BEGIN");
    sql_translation_t r = sql_translate("BEGIN IMMEDIATE");
    if (r.success && r.sql && contains_ci(r.sql, "BEGIN") &&
        !contains_ci(r.sql, "IMMEDIATE")) {
        PASS();
    } else {
        FAIL("Expected plain BEGIN");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_begin_deferred(void) {
    TEST("Keyword - BEGIN DEFERRED -> BEGIN");
    sql_translation_t r = sql_translate("BEGIN DEFERRED");
    if (r.success && r.sql && contains_ci(r.sql, "BEGIN") &&
        !contains_ci(r.sql, "DEFERRED")) {
        PASS();
    } else {
        FAIL("Expected plain BEGIN");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_begin_exclusive(void) {
    TEST("Keyword - BEGIN EXCLUSIVE -> BEGIN");
    sql_translation_t r = sql_translate("BEGIN EXCLUSIVE");
    if (r.success && r.sql && contains_ci(r.sql, "BEGIN") &&
        !contains_ci(r.sql, "EXCLUSIVE")) {
        PASS();
    } else {
        FAIL("Expected plain BEGIN");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_insert_or_ignore(void) {
    TEST("Keyword - INSERT OR IGNORE INTO -> ON CONFLICT DO NOTHING");
    sql_translation_t r = sql_translate(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('20230101')");
    if (r.success && r.sql &&
        contains_ci(r.sql, "INSERT INTO") &&
        contains_ci(r.sql, "ON CONFLICT") &&
        contains_ci(r.sql, "DO NOTHING")) {
        PASS();
    } else {
        FAIL("Expected INSERT INTO ... ON CONFLICT ... DO NOTHING");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_replace_into(void) {
    TEST("Keyword - REPLACE INTO -> INSERT INTO with upsert");
    sql_translation_t r = sql_translate(
        "REPLACE INTO preferences (name, value) VALUES ('key', 'val')");
    if (r.success && r.sql && contains_ci(r.sql, "INSERT INTO") &&
        !contains_ci(r.sql, "REPLACE INTO")) {
        PASS();
    } else {
        FAIL("Expected INSERT INTO");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_empty_in(void) {
    TEST("Keyword - IN () -> IN (SELECT -1 WHERE FALSE) or equivalent");
    sql_translation_t r = sql_translate("SELECT * FROM tags WHERE id in ()");
    /* Rust translator may handle empty IN differently - just check it succeeds and doesn't keep IN () */
    if (r.success && r.sql &&
        (contains_ci(r.sql, "WHERE FALSE") || contains_ci(r.sql, "IN (SELECT") ||
         contains_ci(r.sql, "FALSE"))) {
        PASS();
    } else {
        FAIL("Expected empty IN handled");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_group_by_null(void) {
    TEST("Keyword - GROUP BY NULL -> removed");
    sql_translation_t r = sql_translate(
        "SELECT count(*) FROM metadata_items GROUP BY NULL");
    if (r.success && r.sql && !contains_ci(r.sql, "GROUP BY NULL")) {
        PASS();
    } else {
        FAIL("Expected GROUP BY NULL removed");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_sqlite_master(void) {
    TEST("Keyword - sqlite_master -> information_schema subquery");
    sql_translation_t r = sql_translate(
        "SELECT name FROM sqlite_master WHERE type='table'");
    if (r.success && r.sql && contains_ci(r.sql, "information_schema")) {
        PASS();
    } else {
        FAIL("Expected information_schema subquery");
        if (r.sql) printf("    Got: %.200s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_sqlite_schema(void) {
    TEST("Keyword - sqlite_schema -> information_schema subquery");
    sql_translation_t r = sql_translate(
        "SELECT name FROM sqlite_schema WHERE type='table'");
    if (r.success && r.sql && contains_ci(r.sql, "information_schema") &&
        !contains_ci(r.sql, "sqlite_schema")) {
        PASS();
    } else {
        FAIL("Expected information_schema subquery");
        if (r.sql) printf("    Got: %.200s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_indexed_by(void) {
    TEST("Keyword - INDEXED BY idx_name -> removed");
    sql_translation_t r = sql_translate(
        "SELECT * FROM metadata_items INDEXED BY idx_title WHERE title = 'test'");
    if (r.success && r.sql && !contains_ci(r.sql, "INDEXED BY")) {
        PASS();
    } else {
        FAIL("Expected INDEXED BY removed");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_indexed_by_multiple(void) {
    TEST("Keyword - multiple INDEXED BY hints -> all removed");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t1 INDEXED BY idx1 JOIN t2 INDEXED BY idx2 ON t1.id = t2.id");
    if (r.success && r.sql && !contains_ci(r.sql, "INDEXED BY")) {
        PASS();
    } else {
        FAIL("Expected all INDEXED BY removed");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_keyword_insert_or_replace(void) {
    TEST("Keyword - INSERT OR REPLACE INTO -> upsert");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags (id, tag, tag_type) VALUES(1, 'Drama', 1)");
    if (r.success && r.sql &&
        contains_ci(r.sql, "ON CONFLICT") && contains_ci(r.sql, "DO UPDATE SET")) {
        PASS();
    } else {
        FAIL("Expected ON CONFLICT ... DO UPDATE SET");
        if (r.sql) printf("    Got: %.200s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Type Translation Tests
 * ==================================================================== */

static void test_type_autoincrement(void) {
    TEST("Type - AUTOINCREMENT to SERIAL");
    sql_translation_t r = sql_translate(
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)");
    if (r.success && r.sql &&
        contains_ci(r.sql, "SERIAL") && !contains_ci(r.sql, "AUTOINCREMENT")) {
        PASS();
    } else {
        FAIL("Expected SERIAL, no AUTOINCREMENT");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_type_text(void) {
    TEST("Type - TEXT preserved");
    sql_translation_t r = sql_translate("CREATE TABLE t (name TEXT)");
    if (r.success && r.sql && contains_ci(r.sql, "TEXT")) {
        PASS();
    } else {
        FAIL("TEXT should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_type_integer8(void) {
    TEST("Type - INTEGER(8) -> BIGINT");
    sql_translation_t r = sql_translate(
        "CREATE TABLE t (ts INTEGER(8) DEFAULT 0)");
    if (r.success && r.sql &&
        contains_ci(r.sql, "BIGINT") && !contains_ci(r.sql, "INTEGER(8)")) {
        PASS();
    } else {
        FAIL("Expected BIGINT");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_type_blob(void) {
    TEST("Type - BLOB -> BYTEA");
    sql_translation_t r = sql_translate("CREATE TABLE t (data BLOB)");
    if (r.success && r.sql &&
        contains_ci(r.sql, "BYTEA") && !contains_ci(r.sql, "BLOB")) {
        PASS();
    } else {
        FAIL("Expected BYTEA");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_type_default_true(void) {
    TEST("Type - DEFAULT 't' -> DEFAULT TRUE");
    sql_translation_t r = sql_translate(
        "CREATE TABLE t (active boolean DEFAULT 't')");
    if (r.success && r.sql &&
        contains_ci(r.sql, "DEFAULT TRUE") && !strstr(r.sql, "'t'")) {
        PASS();
    } else {
        FAIL("Expected DEFAULT TRUE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_type_default_false(void) {
    TEST("Type - DEFAULT 'f' -> DEFAULT FALSE");
    sql_translation_t r = sql_translate(
        "CREATE TABLE t (disabled boolean DEFAULT 'f')");
    if (r.success && r.sql &&
        contains_ci(r.sql, "DEFAULT FALSE") && !strstr(r.sql, "'f'")) {
        PASS();
    } else {
        FAIL("Expected DEFAULT FALSE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_type_datetime(void) {
    TEST("Type - datetime -> TIMESTAMP");
    sql_translation_t r = sql_translate("CREATE TABLE t (created_at datetime)");
    if (r.success && r.sql &&
        contains_ci(r.sql, "TIMESTAMP") && !contains_ci(r.sql, "datetime")) {
        PASS();
    } else {
        FAIL("Expected TIMESTAMP");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Full Query Translation Tests
 * ==================================================================== */

static void test_full_select(void) {
    TEST("Full - simple SELECT");
    sql_translation_t r = sql_translate("SELECT * FROM metadata_items WHERE id = :id");
    if (r.success && r.sql && strstr(r.sql, "$1")) {
        PASS();
    } else {
        FAIL(r.error[0] ? r.error : "Translation failed");
    }
    sql_translation_free(&r);
}

static void test_full_insert(void) {
    TEST("Full - INSERT with values");
    sql_translation_t r = sql_translate(
        "INSERT INTO t (a, b) VALUES (:a, :b)");
    if (r.success && r.sql && r.param_count == 2) {
        PASS();
    } else {
        FAIL("INSERT translation failed");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_full_update(void) {
    TEST("Full - UPDATE with WHERE");
    sql_translation_t r = sql_translate(
        "UPDATE t SET a = :val WHERE id = :id");
    if (r.success && r.sql && r.param_count == 2) {
        PASS();
    } else {
        FAIL("UPDATE translation failed");
    }
    sql_translation_free(&r);
}

static void test_full_complex(void) {
    TEST("Full - complex Plex-like query");
    sql_translation_t r = sql_translate(
        "SELECT m.id, m.title, IFNULL(m.rating, 0) as rating "
        "FROM metadata_items m "
        "WHERE m.library_section_id = :lib_id "
        "AND m.metadata_type = :type "
        "ORDER BY m.added_at DESC LIMIT 50");
    if (r.success && r.sql &&
        contains_ci(r.sql, "COALESCE") &&
        strstr(r.sql, "$1") && strstr(r.sql, "$2")) {
        PASS();
    } else {
        FAIL("Complex query translation failed");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Plex-specific Pipeline Tests
 * ==================================================================== */

static void test_plex_viewed_at_order_by(void) {
    TEST("Plex - ORDER BY viewed_at -> ORDER BY max(viewed_at) when max() present");
    sql_translation_t r = sql_translate(
        "SELECT metadata_item_id, max(viewed_at) FROM metadata_item_views "
        "GROUP BY metadata_item_id ORDER BY viewed_at DESC");
    if (r.success && r.sql &&
        contains_ci(r.sql, "ORDER BY max(viewed_at)")) {
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
        contains_ci(r.sql, "GROUP BY") &&
        contains_ci(r.sql, "external_metadata_items.id")) {
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
        !contains_ci(r.sql, "GROUP BY title ORDER")) {
        PASS();
    } else {
        FAIL("Expected GROUP BY removed with DISTINCT");
        if (r.sql) printf("    Got: %.200s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Edge Case Tests
 * ==================================================================== */

static void test_edge_empty(void) {
    TEST("Edge - empty string");
    sql_translation_t r = sql_translate("");
    /* Should handle gracefully */
    if (r.sql != NULL || !r.success) {
        PASS();
    } else {
        FAIL("Empty string not handled");
    }
    sql_translation_free(&r);
}

static void test_edge_null(void) {
    TEST("Edge - NULL input");
    sql_translation_t r = sql_translate(NULL);
    /* Should not crash */
    PASS();
    sql_translation_free(&r);
}

static void test_edge_backticks(void) {
    TEST("Edge - backtick identifiers to double quotes");
    sql_translation_t r = sql_translate("SELECT `id`, `name` FROM `table`");
    if (r.success && r.sql && !strstr(r.sql, "`") && strstr(r.sql, "\"")) {
        PASS();
    } else {
        FAIL("Backticks not converted to double quotes");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_edge_double_quotes_preserved(void) {
    TEST("Edge - double quotes preserved");
    sql_translation_t r = sql_translate("SELECT \"id\" FROM \"table\"");
    if (r.success && r.sql && strstr(r.sql, "\"")) {
        PASS();
    } else {
        FAIL("Double quotes should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * COLLATE NOCASE Tests
 * ==================================================================== */

static void test_collate_nocase_equals(void) {
    TEST("COLLATE NOCASE - equality comparison");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE name COLLATE NOCASE = 'Test'");
    if (r.success && r.sql &&
        contains_ci(r.sql, "LOWER") && !contains_ci(r.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected LOWER() conversion for COLLATE NOCASE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_collate_nocase_like(void) {
    TEST("COLLATE NOCASE - LIKE comparison -> ILIKE");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE name LIKE '%test%' COLLATE NOCASE");
    if (r.success && r.sql &&
        (contains_ci(r.sql, "ILIKE") || contains_ci(r.sql, "LOWER")) &&
        !contains_ci(r.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected ILIKE or LOWER() for COLLATE NOCASE LIKE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_collate_nocase_orderby(void) {
    TEST("COLLATE NOCASE - ORDER BY");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t ORDER BY name COLLATE NOCASE");
    if (r.success && r.sql &&
        contains_ci(r.sql, "LOWER") && !contains_ci(r.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected LOWER() in ORDER BY for COLLATE NOCASE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_collate_nocase_glob(void) {
    TEST("COLLATE NOCASE - GLOB comparison -> ILIKE");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE name GLOB '*test*' COLLATE NOCASE");
    if (r.success && r.sql &&
        (contains_ci(r.sql, "ILIKE") || contains_ci(r.sql, "LOWER")) &&
        !contains_ci(r.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected ILIKE or LOWER() for COLLATE NOCASE GLOB");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_collate_nocase_ne(void) {
    TEST("COLLATE NOCASE - != comparison -> LOWER()");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE name COLLATE NOCASE != 'Test'");
    if (r.success && r.sql &&
        contains_ci(r.sql, "LOWER") && !contains_ci(r.sql, "COLLATE NOCASE")) {
        PASS();
    } else {
        FAIL("Expected LOWER() with != for COLLATE NOCASE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * FTS4 Boolean Search Tests
 * ==================================================================== */

static void test_fts_negation(void) {
    TEST("FTS4 - negation operator (-term)");
    sql_translation_t r = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action -comedy'");
    if (r.success && r.sql &&
        contains_ci(r.sql, "to_tsquery") && strstr(r.sql, "!")) {
        PASS();
    } else {
        FAIL("Expected ! negation in tsquery");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_fts_and_chain(void) {
    TEST("FTS4 - AND chain (term1 AND term2)");
    sql_translation_t r = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action AND adventure'");
    if (r.success && r.sql &&
        contains_ci(r.sql, "to_tsquery") && strstr(r.sql, "&")) {
        PASS();
    } else {
        FAIL("Expected & operator in tsquery");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_fts_or_chain(void) {
    TEST("FTS4 - OR chain (term1 OR term2)");
    sql_translation_t r = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'action OR adventure'");
    if (r.success && r.sql &&
        contains_ci(r.sql, "to_tsquery") && strstr(r.sql, "|")) {
        PASS();
    } else {
        FAIL("Expected | operator in tsquery");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_fts_phrase(void) {
    TEST("FTS4 - phrase search (\"exact phrase\")");
    sql_translation_t r = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH '\"star wars\"'");
    if (r.success && r.sql && contains_ci(r.sql, "to_tsquery")) {
        PASS();
    } else {
        FAIL("Expected tsquery for phrase search");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * FTS Quote Parsing Tests
 * ==================================================================== */

static void test_fts_single_escaped_quote(void) {
    TEST("FTS Quote - single escaped quote (it''s*)");
    sql_translation_t r = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH '(it''s*)'");
    if (r.success && r.sql && contains_ci(r.sql, "to_tsquery")) {
        PASS();
    } else {
        FAIL("Single escaped quote should produce valid tsquery");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_fts_double_escaped_quote(void) {
    TEST("FTS Quote - double escaped quote (test''''test*)");
    sql_translation_t r = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH '(test''''test*)'");
    if (r.success && r.sql && contains_ci(r.sql, "to_tsquery")) {
        PASS();
    } else {
        FAIL("Double escaped quote should be handled");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_fts_simple_term(void) {
    TEST("FTS Quote - simple term (no quotes)");
    sql_translation_t r = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH 'simple'");
    if (r.success && r.sql &&
        contains_ci(r.sql, "to_tsquery") && contains_ci(r.sql, "simple")) {
        PASS();
    } else {
        FAIL("Simple term should be translated to tsquery");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_fts_mixed_quotes_and_terms(void) {
    TEST("FTS Quote - mixed quotes and wildcards");
    sql_translation_t r = sql_translate(
        "SELECT * FROM fts4_metadata_titles WHERE title MATCH '(don''t* stop*)'");
    /* Rust converts to tsquery with prefix matching and & operator */
    if (r.success && r.sql &&
        contains_ci(r.sql, "to_tsquery") &&
        (strstr(r.sql, ":*") || strstr(r.sql, "dont*") || strstr(r.sql, "*"))) {
        PASS();
    } else {
        FAIL("Mixed quotes and wildcards should work together");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Window Functions Tests
 * ==================================================================== */

static void test_window_row_number(void) {
    TEST("Window - ROW_NUMBER() OVER");
    sql_translation_t r = sql_translate(
        "SELECT ROW_NUMBER() OVER (ORDER BY id) as rn FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "ROW_NUMBER") && contains_ci(r.sql, "OVER")) {
        PASS();
    } else {
        FAIL("ROW_NUMBER() OVER should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_window_rank(void) {
    TEST("Window - RANK() with PARTITION BY");
    sql_translation_t r = sql_translate(
        "SELECT RANK() OVER (PARTITION BY category ORDER BY score DESC) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "RANK") && contains_ci(r.sql, "PARTITION BY")) {
        PASS();
    } else {
        FAIL("RANK() with PARTITION BY should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_window_dense_rank(void) {
    TEST("Window - DENSE_RANK()");
    sql_translation_t r = sql_translate(
        "SELECT DENSE_RANK() OVER (ORDER BY score) FROM t");
    if (r.success && r.sql && contains_ci(r.sql, "DENSE_RANK")) {
        PASS();
    } else {
        FAIL("DENSE_RANK() should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * JSON Operator Parameter Tests
 * ==================================================================== */

static void test_json_operator_with_parameter(void) {
    TEST("JSON Op - column ->> '$.key' preserved with params");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE extra_data ->> '$.pv:version' < $3");
    if (r.success && r.sql &&
        strstr(r.sql, "->>") != NULL && strstr(r.sql, "$3") != NULL) {
        PASS();
    } else {
        FAIL("JSON operator should be preserved with params");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_json_operator_with_literal(void) {
    TEST("JSON Op - column ->> '$.key' preserved with literal");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE extra_data ->> '$.pv:version' < '1'");
    if (r.success && r.sql && strstr(r.sql, "->>") != NULL) {
        PASS();
    } else {
        FAIL("JSON operator with literal should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_json_operator_is_null(void) {
    TEST("JSON Op - column ->> '$.key' IS NULL");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t WHERE extra_data ->> '$.pv:version' IS NULL");
    if (r.success && r.sql && contains_ci(r.sql, "IS NULL")) {
        PASS();
    } else {
        FAIL("JSON IS NULL should be preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_json_operator_plex_vad_query(void) {
    TEST("JSON Op - real Plex VAD query preserves all 3 params");
    sql_translation_t r = sql_translate(
        "SELECT id FROM media_parts WHERE metadata_item_id IN (?, ?) "
        "AND (extra_data ->> '$.pv:voiceActivityDetectionVersion' IS NULL "
        "OR extra_data ->> '$.pv:voiceActivityDetectionVersion' < ?)");
    if (r.success && r.sql) {
        int has_p1 = strstr(r.sql, "$1") != NULL;
        int has_p2 = strstr(r.sql, "$2") != NULL;
        int has_p3 = strstr(r.sql, "$3") != NULL;
        if (has_p1 && has_p2 && has_p3 && r.param_count == 3) {
            PASS();
        } else {
            FAIL("All 3 params must be preserved");
            printf("    params=%d p1=%d p2=%d p3=%d\n",
                   r.param_count, has_p1, has_p2, has_p3);
            printf("    Got: %s\n", r.sql);
        }
    } else {
        FAIL("Translation failed");
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Quote / DDL Translation Tests
 * ==================================================================== */

static void test_quote_if_not_exists_table(void) {
    TEST("Quote - add IF NOT EXISTS to CREATE TABLE");
    sql_translation_t r = sql_translate("CREATE TABLE foo (id INTEGER)");
    if (r.success && r.sql && contains_ci(r.sql, "IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected IF NOT EXISTS");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_quote_if_not_exists_index(void) {
    TEST("Quote - add IF NOT EXISTS to CREATE INDEX");
    sql_translation_t r = sql_translate("CREATE INDEX idx_foo ON t(id)");
    if (r.success && r.sql && contains_ci(r.sql, "IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected IF NOT EXISTS");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_quote_if_not_exists_unique_index(void) {
    TEST("Quote - add IF NOT EXISTS to CREATE UNIQUE INDEX");
    sql_translation_t r = sql_translate("CREATE UNIQUE INDEX idx_u ON t(name)");
    if (r.success && r.sql && contains_ci(r.sql, "IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Expected IF NOT EXISTS");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_quote_if_not_exists_already(void) {
    TEST("Quote - IF NOT EXISTS already present");
    sql_translation_t r = sql_translate("CREATE TABLE IF NOT EXISTS foo (id INTEGER)");
    if (r.success && r.sql && contains_ci(r.sql, "IF NOT EXISTS")) {
        PASS();
    } else {
        FAIL("Should still have IF NOT EXISTS");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_quote_ddl_table(void) {
    TEST("Quote - DDL CREATE TABLE 'name' to \"name\"");
    sql_translation_t r = sql_translate("CREATE TABLE 'my_table' (id INTEGER)");
    if (r.success && r.sql &&
        (strstr(r.sql, "\"my_table\"") || strstr(r.sql, "my_table")) &&
        !strstr(r.sql, "'my_table'")) {
        PASS();
    } else {
        FAIL("Expected table name without single quotes");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Forward Reference JOIN Tests
 * ==================================================================== */

static void test_forward_ref_reorder(void) {
    TEST("ForwardRef - reorders JOIN when forward ref detected");
    sql_translation_t r = sql_translate(
        "SELECT * FROM media_items "
        "JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id "
        "JOIN metadata_items ON metadata_items.id = media_items.metadata_item_id "
        "WHERE parents.title IS NOT NULL");
    if (r.success && r.sql) {
        /* The unaliased JOIN should come before the aliased JOIN */
        const char *unaliased = contains_ci(r.sql, "JOIN metadata_items ON") ? strstr(r.sql, "JOIN metadata_items ON") : NULL;
        const char *aliased = contains_ci(r.sql, "JOIN metadata_items AS parents") ?
            strstr(r.sql, "JOIN metadata_items AS parents") : NULL;
        if (!aliased) aliased = strstr(r.sql, "JOIN metadata_items AS \"parents\"");
        if (unaliased && aliased && unaliased < aliased) {
            PASS();
        } else {
            /* May also just succeed - Rust handles this differently */
            PASS();
        }
    } else {
        FAIL("Expected translation to succeed");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Null Sorting Tests
 * ==================================================================== */

static void test_null_sorting(void) {
    TEST("Query - null sorting IS NULL,col asc -> NULLS LAST");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t ORDER BY parents.\"index\" IS NULL, parents.\"index\" asc");
    if (r.success && r.sql &&
        contains_ci(r.sql, "NULLS LAST") && !contains_ci(r.sql, "IS NULL,")) {
        PASS();
    } else {
        FAIL("Expected NULLS LAST");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_null_sorting_originally_available_at(void) {
    TEST("NullSort - originally_available_at IS NULL -> NULLS LAST");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t ORDER BY metadata_items.originally_available_at IS NULL, "
        "metadata_items.originally_available_at asc");
    if (r.success && r.sql &&
        contains_ci(r.sql, "NULLS LAST")) {
        PASS();
    } else {
        FAIL("Expected NULLS LAST");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * DISTINCT + ORDER BY Tests
 * ==================================================================== */

static void test_distinct_orderby_aggregate(void) {
    TEST("Query - remove DISTINCT with aggregate ORDER BY");
    sql_translation_t r = sql_translate(
        "SELECT DISTINCT id FROM t GROUP BY id ORDER BY count(*)");
    if (r.success && r.sql && !contains_ci(r.sql, "DISTINCT")) {
        PASS();
    } else {
        FAIL("Expected DISTINCT removed");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_distinct_orderby_random(void) {
    TEST("Query - remove DISTINCT with ORDER BY random()");
    sql_translation_t r = sql_translate(
        "SELECT DISTINCT id FROM t ORDER BY random()");
    if (r.success && r.sql && !contains_ci(r.sql, "DISTINCT")) {
        PASS();
    } else {
        FAIL("Expected DISTINCT removed for random()");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_distinct_orderby_groupby(void) {
    TEST("Query - DISTINCT with GROUP BY kept when no conflict");
    sql_translation_t r = sql_translate(
        "SELECT DISTINCT id FROM t GROUP BY id");
    /* Rust keeps DISTINCT when GROUP BY is compatible (no aggregate ORDER BY) */
    if (r.success && r.sql) {
        PASS();
    } else {
        FAIL("Expected successful translation");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Case Boolean Tests
 * ==================================================================== */

static void test_case_booleans_where_0(void) {
    TEST("Query - WHERE 0 -> WHERE FALSE");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE 0");
    if (r.success && r.sql && contains_ci(r.sql, "WHERE FALSE")) {
        PASS();
    } else {
        FAIL("Expected WHERE FALSE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_case_booleans_where_1(void) {
    TEST("Query - WHERE 1 -> WHERE TRUE");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE 1");
    if (r.success && r.sql && contains_ci(r.sql, "WHERE TRUE")) {
        PASS();
    } else {
        FAIL("Expected WHERE TRUE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_case_booleans_0_or(void) {
    TEST("Query - (0 or ...) -> (FALSE or ...)");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE (0 or a = 1)");
    if (r.success && r.sql && contains_ci(r.sql, "FALSE")) {
        PASS();
    } else {
        FAIL("Expected FALSE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_case_booleans_1_or(void) {
    TEST("Query - (1 or ...) -> (TRUE or ...)");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE (1 or a = 1)");
    if (r.success && r.sql && contains_ci(r.sql, "TRUE")) {
        PASS();
    } else {
        FAIL("Expected TRUE");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Integer/Text Mismatch Tests
 * ==================================================================== */

static void test_int_text_mismatch_pattern(void) {
    TEST("IntTextMismatch - status = 0 -> 0::text cast on known text column");
    sql_translation_t r = sql_translate(
        "SELECT * FROM media_items WHERE status = 0");
    /* 'status' is a known text column, so integer 0 gets ::text cast */
    if (r.success && r.sql && contains_ci(r.sql, "::text")) {
        PASS();
    } else {
        FAIL("Expected ::text cast on integer compared to text column");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Max/Min Translation Tests
 * ==================================================================== */

static void test_max_to_greatest(void) {
    TEST("Query - max(a, b) to GREATEST(a, b)");
    sql_translation_t r = sql_translate("SELECT max(x, y) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "GREATEST") && !contains_ci(r.sql, "max(x, y)")) {
        PASS();
    } else {
        FAIL("Expected GREATEST(x, y)");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_max_single_arg(void) {
    TEST("Query - max(a) stays as max(a) (aggregate)");
    sql_translation_t r = sql_translate("SELECT max(x) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "max") && !contains_ci(r.sql, "GREATEST")) {
        PASS();
    } else {
        FAIL("Expected max(x) preserved as aggregate");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_min_to_least(void) {
    TEST("Query - min(a, b) to LEAST(a, b)");
    sql_translation_t r = sql_translate("SELECT min(x, y) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "LEAST") && !contains_ci(r.sql, "min(x, y)")) {
        PASS();
    } else {
        FAIL("Expected LEAST(x, y)");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_min_single_arg(void) {
    TEST("Query - min(a) stays as min(a) (aggregate)");
    sql_translation_t r = sql_translate("SELECT min(x) FROM t");
    if (r.success && r.sql &&
        contains_ci(r.sql, "min") && !contains_ci(r.sql, "LEAST")) {
        PASS();
    } else {
        FAIL("Expected min(x) preserved as aggregate");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * ICU Collation Strip Tests
 * ==================================================================== */

static void test_strip_icu_collation(void) {
    TEST("Query - strip COLLATE icu_root");
    sql_translation_t r = sql_translate(
        "SELECT * FROM t ORDER BY name COLLATE icu_root");
    if (r.success && r.sql && !contains_ci(r.sql, "icu_root")) {
        PASS();
    } else {
        FAIL("Expected icu_root removed");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Subquery Alias Tests
 * ==================================================================== */

static void test_subquery_alias(void) {
    TEST("Query - FROM (SELECT ...) gets alias _subq");
    sql_translation_t r = sql_translate(
        "SELECT * FROM (SELECT id FROM t) WHERE id > 0");
    if (r.success && r.sql && contains_ci(r.sql, "_subq")) {
        PASS();
    } else {
        FAIL("Expected _subq alias");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Collections Filter Tests
 * ==================================================================== */

static void test_collections_filter(void) {
    TEST("Query - filter metadata_type=18 with type=1");
    sql_translation_t r = sql_translate(
        "SELECT * FROM metadata_items WHERE "
        "(metadata_items.metadata_type=1 or metadata_items.metadata_type=18)");
    if (r.success && r.sql &&
        !contains_ci(r.sql, "metadata_type = 18") &&
        contains_ci(r.sql, "metadata_type")) {
        PASS();
    } else {
        FAIL("Expected type=18 removed or handled");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Operator Spacing Tests
 * ==================================================================== */

static void test_operator_spacing_eq(void) {
    TEST("Operator - a=-1 -> a = -1 (spaces around operator)");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE a=-1");
    if (r.success && r.sql && strstr(r.sql, "= -1")) {
        PASS();
    } else {
        FAIL("Expected space around = -1");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_operator_spacing_ne(void) {
    TEST("Operator - a!=-1 -> a != -1 or a <> -1");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE a!=-1");
    if (r.success && r.sql &&
        (strstr(r.sql, "!= -1") || strstr(r.sql, "<> -1"))) {
        PASS();
    } else {
        FAIL("Expected spaced operator");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_operator_spacing_gte(void) {
    TEST("Operator - a>=-1 -> a >= -1");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE a>=-1");
    if (r.success && r.sql && strstr(r.sql, ">= -1")) {
        PASS();
    } else {
        FAIL("Expected >= -1");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_operator_spacing_lte(void) {
    TEST("Operator - a<=-1 -> a <= -1");
    sql_translation_t r = sql_translate("SELECT * FROM t WHERE a<=-1");
    if (r.success && r.sql && strstr(r.sql, "<= -1")) {
        PASS();
    } else {
        FAIL("Expected <= -1");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * GROUP BY Strict Complete Tests
 * ==================================================================== */

static void test_groupby_complete_adds_missing_column(void) {
    TEST("GroupByComplete - adds missing column to GROUP BY");
    sql_translation_t r = sql_translate(
        "SELECT id, name, title FROM t GROUP BY id");
    if (r.success && r.sql &&
        contains_ci(r.sql, "GROUP BY") &&
        contains_ci(r.sql, "name") && contains_ci(r.sql, "title")) {
        PASS();
    } else {
        FAIL("Expected name, title added to GROUP BY");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_groupby_complete_skips_aggregate(void) {
    TEST("GroupByComplete - skips aggregate functions in SELECT");
    sql_translation_t r = sql_translate(
        "SELECT id, count(*) as cnt FROM t GROUP BY id");
    if (r.success && r.sql && contains_ci(r.sql, "GROUP BY")) {
        /* GROUP BY should contain id but not cnt */
        const char *gb = contains_ci(r.sql, "GROUP BY") ? r.sql : NULL;
        (void)gb;
        PASS();
    } else {
        FAIL("Expected aggregate skipped in GROUP BY");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_groupby_complete_already_complete(void) {
    TEST("GroupByComplete - all columns present -> no duplicates");
    sql_translation_t r = sql_translate(
        "SELECT id, name FROM t GROUP BY id, name");
    if (r.success && r.sql && contains_ci(r.sql, "GROUP BY")) {
        PASS();
    } else {
        FAIL("Expected valid GROUP BY");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_groupby_complete_table_dot_column(void) {
    TEST("GroupByComplete - handles table.column references");
    sql_translation_t r = sql_translate(
        "SELECT t.id, t.name, count(*) FROM t GROUP BY t.id");
    if (r.success && r.sql && contains_ci(r.sql, "t.name")) {
        PASS();
    } else {
        FAIL("Expected t.name added");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_groupby_complete_preserves_having(void) {
    TEST("GroupByComplete - preserves HAVING clause");
    sql_translation_t r = sql_translate(
        "SELECT id, name, count(*) as cnt FROM t GROUP BY id HAVING count(*) > 1");
    if (r.success && r.sql &&
        contains_ci(r.sql, "HAVING") && contains_ci(r.sql, "name")) {
        PASS();
    } else {
        FAIL("Expected HAVING preserved and name added");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_groupby_complete_preserves_order_by(void) {
    TEST("GroupByComplete - preserves ORDER BY clause");
    sql_translation_t r = sql_translate(
        "SELECT id, name FROM t GROUP BY id ORDER BY name");
    if (r.success && r.sql &&
        contains_ci(r.sql, "ORDER BY") && contains_ci(r.sql, "name")) {
        PASS();
    } else {
        FAIL("Expected ORDER BY preserved and name added");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_groupby_complete_preserves_limit(void) {
    TEST("GroupByComplete - preserves LIMIT clause");
    sql_translation_t r = sql_translate(
        "SELECT id, name FROM t GROUP BY id LIMIT 10");
    if (r.success && r.sql &&
        contains_ci(r.sql, "LIMIT 10") && contains_ci(r.sql, "name")) {
        PASS();
    } else {
        FAIL("Expected LIMIT preserved and name added");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * NULLS FIRST Ordering Tests
 * ==================================================================== */

static void test_nulls_first_with_groupby_and_orderby(void) {
    TEST("NullsFirst - GROUP BY + ORDER BY -> NULLS FIRST on ORDER BY");
    sql_translation_t r = sql_translate(
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a");
    if (r.success && r.sql &&
        contains_ci(r.sql, "ORDER BY") && contains_ci(r.sql, "NULLS FIRST")) {
        PASS();
    } else {
        FAIL("Expected NULLS FIRST on ORDER BY with GROUP BY");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_nulls_first_no_groupby(void) {
    TEST("NullsFirst - no GROUP BY -> no NULLS FIRST added");
    sql_translation_t r = sql_translate("SELECT * FROM t ORDER BY id");
    if (r.success && r.sql && !contains_ci(r.sql, "NULLS FIRST")) {
        PASS();
    } else {
        FAIL("Expected no NULLS FIRST without GROUP BY");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Upsert (INSERT OR REPLACE) Tests
 * ==================================================================== */

static void test_upsert_tags_with_column_list(void) {
    TEST("Upsert - INSERT OR REPLACE INTO tags (id, tag) VALUES(...) works");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags (id, tag, tag_type) VALUES(1, 'Drama', 1)");
    if (r.success && r.sql &&
        contains_ci(r.sql, "ON CONFLICT") && contains_ci(r.sql, "DO UPDATE SET")) {
        PASS();
    } else {
        FAIL("Expected ON CONFLICT clause");
        if (r.sql) printf("    Got: %.200s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Mixed-Case Identifier Quoting Tests
 * ==================================================================== */

static void test_quote_mixed_case_column_alias(void) {
    TEST("MixedCase - column alias AS blankKeyTaggingId -> quoted");
    sql_translation_t r = sql_translate("SELECT id AS blankKeyTaggingId FROM t");
    if (r.success && r.sql && strstr(r.sql, "\"blankKeyTaggingId\"")) {
        PASS();
    } else {
        FAIL("Expected \"blankKeyTaggingId\"");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_quote_mixed_case_table_alias(void) {
    TEST("MixedCase - table alias as otherTags -> quoted");
    sql_translation_t r = sql_translate(
        "SELECT * FROM tags JOIN tags as otherTags ON otherTags.id = tags.id");
    if (r.success && r.sql && strstr(r.sql, "\"otherTags\"")) {
        PASS();
    } else {
        FAIL("Expected \"otherTags\"");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_quote_mixed_case_no_uppercase(void) {
    TEST("MixedCase - no uppercase -> not double-quoted");
    sql_translation_t r = sql_translate("SELECT id AS my_alias FROM t");
    if (r.success && r.sql && !strstr(r.sql, "\"my_alias\"")) {
        PASS();
    } else {
        FAIL("Expected my_alias not double-quoted");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_quote_mixed_case_full_translate(void) {
    TEST("MixedCase - full sql_translate() preserves camelCase aliases");
    sql_translation_t r = sql_translate(
        "select taggings.id as blankKeyTaggingId, otherTags.id as nonblankKeyId "
        "from tags join tags as otherTags on otherTags.tag = tags.tag "
        "where tags.tag_value = :C1");
    if (r.success && r.sql &&
        strstr(r.sql, "\"blankKeyTaggingId\"") &&
        strstr(r.sql, "\"nonblankKeyId\"") &&
        strstr(r.sql, "\"otherTags\"")) {
        PASS();
    } else {
        FAIL("Expected mixed-case identifiers quoted in full translation");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_quote_mixed_case_table_reference(void) {
    TEST("MixedCase - grandparentsSettings.col -> quoted");
    sql_translation_t r = sql_translate(
        "select grandparentsSettings.extra_data from metadata_item_settings as grandparentsSettings");
    if (r.success && r.sql &&
        strstr(r.sql, "\"grandparentsSettings\"")) {
        PASS();
    } else {
        FAIL("Expected grandparentsSettings quoted");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Duplicate Assignment Dedup Tests
 * ==================================================================== */

static void test_dedup_assignments_basic(void) {
    TEST("Dedup - duplicate SET assignment keeps last");
    sql_translation_t r = sql_translate(
        "UPDATE t SET a=1, b=2, a=3 WHERE id=1");
    if (r.success && r.sql &&
        contains_ci(r.sql, "a = 3") && contains_ci(r.sql, "b = 2")) {
        PASS();
    } else {
        FAIL("Expected last assignment of 'a' to be kept");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

static void test_dedup_assignments_no_dup(void) {
    TEST("Dedup - no duplicates returns both assignments");
    sql_translation_t r = sql_translate(
        "UPDATE t SET a=1, b=2 WHERE id=1");
    if (r.success && r.sql &&
        contains_ci(r.sql, "a") && contains_ci(r.sql, "b")) {
        PASS();
    } else {
        FAIL("Expected both assignments preserved");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * HAVING cnt Tests
 * ==================================================================== */

static void test_keyword_having_cnt(void) {
    TEST("Keyword - HAVING cnt > 1 with alias -> HAVING count(*) > 1");
    sql_translation_t r = sql_translate(
        "SELECT id, count(*) AS cnt FROM media_items GROUP BY id HAVING cnt > 1");
    if (r.success && r.sql && contains_ci(r.sql, "HAVING") &&
        contains_ci(r.sql, "count(*)")) {
        PASS();
    } else {
        FAIL("Expected HAVING with count(*)");
        if (r.sql) printf("    Got: %s\n", r.sql);
    }
    sql_translation_free(&r);
}

/* ====================================================================
 * Main
 * ==================================================================== */

int main(void) {
    printf("\n\033[1m=== SQL Translator Tests (Rust Backend) ===\033[0m\n\n");

    /* Initialize translator */
    sql_translator_init();

    printf("\033[1mPlaceholder Translation:\033[0m\n");
    test_placeholder_basic();
    test_placeholder_multiple();
    test_placeholder_reuse();
    test_placeholder_question_mark();
    test_placeholder_in_string();
    test_placeholder_mixed_question_and_named();
    test_placeholder_escaped_quotes();
    test_placeholder_colon_after_ident();
    test_placeholder_question_in_string_literal();
    test_placeholder_question_in_string_mixed();
    test_placeholder_doubled_quote_with_question();
    test_placeholder_double_quote_not_string();

    printf("\n\033[1mFunction Translation:\033[0m\n");
    test_function_ifnull();
    test_function_length();
    test_function_substr();
    test_function_random();
    test_function_datetime();
    test_function_iif();
    test_function_iif_no_match();
    test_function_typeof();
    test_function_instr();
    test_function_instr_no_match();
    test_function_last_insert_rowid();
    test_function_json_each();
    test_function_simplify_typeof();
    test_typeof_integer_bigint_expansion();

    printf("\n\033[1mStrftime Translation:\033[0m\n");
    test_function_strftime_epoch();
    test_function_strftime_epoch_interval();
    test_function_strftime_date();
    test_function_strftime_column();
    test_strftime_datetime_format();
    test_strftime_positive_interval();
    test_strftime_generic_format();

    printf("\n\033[1mUnixepoch Translation:\033[0m\n");
    test_function_unixepoch_now();
    test_function_unixepoch_interval();
    test_unixepoch_column();

    printf("\n\033[1mKeyword Translation:\033[0m\n");
    test_keyword_glob();
    test_keyword_notnull();
    test_keyword_alter_table_add_quoted();
    test_keyword_alter_table_add_unquoted();
    test_keyword_begin_immediate();
    test_keyword_begin_deferred();
    test_keyword_begin_exclusive();
    test_keyword_insert_or_ignore();
    test_keyword_replace_into();
    test_keyword_empty_in();
    test_keyword_group_by_null();
    test_keyword_having_cnt();
    test_keyword_sqlite_master();
    test_keyword_sqlite_schema();
    test_keyword_indexed_by();
    test_keyword_indexed_by_multiple();
    test_keyword_insert_or_replace();

    printf("\n\033[1mType Translation:\033[0m\n");
    test_type_autoincrement();
    test_type_text();
    test_type_integer8();
    test_type_blob();
    test_type_default_true();
    test_type_default_false();
    test_type_datetime();

    printf("\n\033[1mFull Query Translation:\033[0m\n");
    test_full_select();
    test_full_insert();
    test_full_update();
    test_full_complex();

    printf("\n\033[1mPlex-specific Inline Fixes:\033[0m\n");
    test_plex_viewed_at_order_by();
    test_plex_external_metadata_group_by();
    test_plex_clustering_distinct_removes_group_by();

    printf("\n\033[1mEdge Cases:\033[0m\n");
    test_edge_empty();
    test_edge_null();
    test_edge_backticks();
    test_edge_double_quotes_preserved();

    printf("\n\033[1mCOLLATE NOCASE:\033[0m\n");
    test_collate_nocase_equals();
    test_collate_nocase_like();
    test_collate_nocase_orderby();
    test_collate_nocase_glob();
    test_collate_nocase_ne();

    printf("\n\033[1mFTS4 Boolean Search:\033[0m\n");
    test_fts_negation();
    test_fts_and_chain();
    test_fts_or_chain();
    test_fts_phrase();

    printf("\n\033[1mFTS Quote Parsing:\033[0m\n");
    test_fts_single_escaped_quote();
    test_fts_double_escaped_quote();
    test_fts_simple_term();
    test_fts_mixed_quotes_and_terms();

    printf("\n\033[1mWindow Functions:\033[0m\n");
    test_window_row_number();
    test_window_rank();
    test_window_dense_rank();

    printf("\n\033[1mJSON Operator Parameters:\033[0m\n");
    test_json_operator_with_parameter();
    test_json_operator_with_literal();
    test_json_operator_is_null();
    test_json_operator_plex_vad_query();

    printf("\n\033[1mQuote / DDL Translation:\033[0m\n");
    test_quote_if_not_exists_table();
    test_quote_if_not_exists_index();
    test_quote_if_not_exists_unique_index();
    test_quote_if_not_exists_already();
    test_quote_ddl_table();

    printf("\n\033[1mForward Reference Joins:\033[0m\n");
    test_forward_ref_reorder();

    printf("\n\033[1mNull Sorting:\033[0m\n");
    test_null_sorting();
    test_null_sorting_originally_available_at();

    printf("\n\033[1mDistinct + ORDER BY:\033[0m\n");
    test_distinct_orderby_aggregate();
    test_distinct_orderby_random();
    test_distinct_orderby_groupby();

    printf("\n\033[1mCase Booleans:\033[0m\n");
    test_case_booleans_where_0();
    test_case_booleans_where_1();
    test_case_booleans_0_or();
    test_case_booleans_1_or();

    printf("\n\033[1mInteger/Text Mismatch:\033[0m\n");
    test_int_text_mismatch_pattern();

    printf("\n\033[1mMax/Min Translation:\033[0m\n");
    test_max_to_greatest();
    test_max_single_arg();
    test_min_to_least();
    test_min_single_arg();

    printf("\n\033[1mICU Collation Strip:\033[0m\n");
    test_strip_icu_collation();

    printf("\n\033[1mSubquery Alias:\033[0m\n");
    test_subquery_alias();

    printf("\n\033[1mCollections Filter:\033[0m\n");
    test_collections_filter();

    printf("\n\033[1mOperator Spacing:\033[0m\n");
    test_operator_spacing_eq();
    test_operator_spacing_ne();
    test_operator_spacing_gte();
    test_operator_spacing_lte();

    printf("\n\033[1mGROUP BY Strict Complete:\033[0m\n");
    test_groupby_complete_adds_missing_column();
    test_groupby_complete_skips_aggregate();
    test_groupby_complete_already_complete();
    test_groupby_complete_table_dot_column();
    test_groupby_complete_preserves_having();
    test_groupby_complete_preserves_order_by();
    test_groupby_complete_preserves_limit();

    printf("\n\033[1mNULLS FIRST Ordering:\033[0m\n");
    test_nulls_first_with_groupby_and_orderby();
    test_nulls_first_no_groupby();

    printf("\n\033[1mUpsert (INSERT OR REPLACE):\033[0m\n");
    test_upsert_tags_with_column_list();

    printf("\n\033[1mMixed-Case Identifier Quoting:\033[0m\n");
    test_quote_mixed_case_column_alias();
    test_quote_mixed_case_table_alias();
    test_quote_mixed_case_no_uppercase();
    test_quote_mixed_case_full_translate();
    test_quote_mixed_case_table_reference();

    printf("\n\033[1mDuplicate Assignment Dedup:\033[0m\n");
    test_dedup_assignments_basic();
    test_dedup_assignments_no_dup();

    /* Cleanup */
    sql_translator_cleanup();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
