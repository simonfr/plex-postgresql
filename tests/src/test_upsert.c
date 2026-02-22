/*
 * Comprehensive unit tests for INSERT OR REPLACE → ON CONFLICT translation
 *
 * All tests use the Rust-only sql_translate() entry point.
 * The Rust translator (sqlparser-rs) handles:
 *   - INSERT OR REPLACE → INSERT INTO ... ON CONFLICT(target) DO UPDATE SET ...
 *   - INSERT OR IGNORE  → INSERT INTO ... ON CONFLICT(target) DO NOTHING
 *   - REPLACE INTO      → same as INSERT OR REPLACE
 *   - Known table conflict targets (tags→id, preferences→name, etc.)
 *   - RETURNING id when any conflict column contains "id" substring
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>

#include "sql_translator.h"

/* ── Test infrastructure ──────────────────────────────────────────────────── */

static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %-60s ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)

/* Case-insensitive substring search */
static int contains_ci(const char *haystack, const char *needle) {
    if (!haystack || !needle) return 0;
    size_t hlen = strlen(haystack);
    size_t nlen = strlen(needle);
    if (nlen > hlen) return 0;
    for (size_t i = 0; i <= hlen - nlen; i++) {
        size_t j;
        for (j = 0; j < nlen; j++) {
            if (tolower((unsigned char)haystack[i + j]) !=
                tolower((unsigned char)needle[j]))
                break;
        }
        if (j == nlen) return 1;
    }
    return 0;
}

/* Case-sensitive substring search */
static int contains_exact(const char *haystack, const char *needle) {
    if (!haystack || !needle) return 0;
    return strstr(haystack, needle) != NULL;
}

/* ── Macro: translate and assert success ──────────────────────────────────── */

#define test_assert(cond, result, label) do { \
    if (cond) { PASS(); } else { \
        char _msg[512]; \
        snprintf(_msg, sizeof(_msg), "%s | Got: %.400s", label, \
                 (result).sql ? (result).sql : "NULL"); \
        FAIL(_msg); \
    } \
} while(0)

/* ========================================================================== */
/* Edge Cases                                                                 */
/* ========================================================================== */

static void test_null_input(void) {
    TEST("NULL input → success=0");
    sql_translation_t r = sql_translate(NULL);
    if (!r.success) { PASS(); } else { FAIL("Expected failure for NULL input"); }
    sql_translation_free(&r);
}

static void test_non_insert(void) {
    TEST("SELECT → returned unchanged");
    sql_translation_t r = sql_translate("SELECT * FROM metadata_items");
    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "SELECT") &&
                !contains_ci(r.sql, "ON CONFLICT"),
                r, "Expected unchanged SELECT");
    sql_translation_free(&r);
}

static void test_plain_insert(void) {
    TEST("Plain INSERT (no OR REPLACE) → unchanged");
    sql_translation_t r = sql_translate(
        "INSERT INTO tags (id, tag) VALUES (1, 'test')");
    test_assert(r.success && r.sql &&
                !contains_ci(r.sql, "ON CONFLICT"),
                r, "Expected no ON CONFLICT");
    sql_translation_free(&r);
}

static void test_no_column_list(void) {
    TEST("INSERT OR REPLACE without column list → ON CONFLICT, no SET cols");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags VALUES (1, 'test', 0)");
    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT") &&
                !contains_ci(r.sql, "OR REPLACE"),
                r, "Expected ON CONFLICT without OR REPLACE");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* Tables with (id) conflict target                                           */
/* ========================================================================== */

static void test_id_table(const char *table_name, const char *display_name) {
    char test_name[128];
    snprintf(test_name, sizeof(test_name), "%s → ON CONFLICT(id)", display_name);
    TEST(test_name);

    char sql[512];
    snprintf(sql, sizeof(sql),
        "INSERT OR REPLACE INTO %s (id, name, created_at) VALUES (1, 'test', 12345)",
        table_name);

    sql_translation_t r = sql_translate(sql);
    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                !contains_ci(r.sql, "OR REPLACE") &&
                contains_ci(r.sql, "INSERT INTO") &&
                contains_ci(r.sql, "RETURNING id"),
                r, "Expected ON CONFLICT(id) DO UPDATE SET ... RETURNING id");
    sql_translation_free(&r);
}

static void test_all_id_tables(void) {
    printf("\n\033[1mTables with (id) conflict target:\033[0m\n");

    test_id_table("metadata_items",             "metadata_items");
    test_id_table("media_items",                "media_items");
    test_id_table("media_parts",                "media_parts");
    test_id_table("media_streams",              "media_streams");
    test_id_table("tags",                       "tags");
    test_id_table("taggings",                   "taggings");
    test_id_table("statistics_media",           "statistics_media");
    test_id_table("statistics_resources",       "statistics_resources");
    test_id_table("play_queue_generators",      "play_queue_generators");
    test_id_table("play_queue_items",           "play_queue_items");
    test_id_table("play_queues",                "play_queues");
    test_id_table("activities",                 "activities");
    test_id_table("accounts",                   "accounts");
    test_id_table("devices",                    "devices");
    test_id_table("directories",               "directories");
    test_id_table("library_sections",           "library_sections");
    test_id_table("locations",                  "locations");
    test_id_table("plugins",                    "plugins");
    test_id_table("media_grabs",               "media_grabs");
    test_id_table("metadata_relations",         "metadata_relations");
    test_id_table("versioned_metadata_items",   "versioned_metadata_items");
    test_id_table("external_metadata_sources",  "external_metadata_sources");
    test_id_table("blobs",                      "blobs");
}

/* ========================================================================== */
/* Tables with composite/unique conflict targets                              */
/* ========================================================================== */

static void test_statistics_bandwidth(void) {
    printf("\n\033[1mTables with composite/unique conflict targets:\033[0m\n");

    TEST("statistics_bandwidth → ON CONFLICT(account_id, device_id, ...)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO statistics_bandwidth "
        "(id, account_id, device_id, timespan, at, lan, bytes) "
        "VALUES (1, 2, 3, 4, 5, 1, 1024)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(account_id, device_id, timespan, at, lan)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                !contains_ci(r.sql, "OR REPLACE"),
                r, "Expected composite conflict target");
    sql_translation_free(&r);
}

static void test_locatables(void) {
    TEST("locatables → ON CONFLICT(location_id, locatable_id, locatable_type)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO locatables "
        "(id, location_id, locatable_id, locatable_type, created_at) "
        "VALUES (1, 10, 20, 'MediaItem', 12345)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(location_id, locatable_id, locatable_type)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected locatables composite conflict");
    sql_translation_free(&r);
}

static void test_location_places(void) {
    TEST("location_places → ON CONFLICT(location_id, guid)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO location_places "
        "(id, location_id, guid, name) "
        "VALUES (1, 10, 'abc-123', 'Home')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(location_id, guid)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected location_places conflict target");
    sql_translation_free(&r);
}

static void test_media_stream_settings(void) {
    TEST("media_stream_settings → ON CONFLICT(media_stream_id, account_id)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO media_stream_settings "
        "(id, media_stream_id, account_id, selected) "
        "VALUES (1, 100, 1, 1)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(media_stream_id, account_id)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected media_stream_settings conflict");
    sql_translation_free(&r);
}

static void test_preferences(void) {
    TEST("preferences → ON CONFLICT(name)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO preferences "
        "(id, name, value) "
        "VALUES (1, 'FriendlyName', 'My Plex')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(name)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected preferences conflict on name");
    sql_translation_free(&r);
}

static void test_metadata_item_settings(void) {
    TEST("metadata_item_settings → ON CONFLICT(account_id, guid)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO metadata_item_settings "
        "(id, account_id, guid, rating, view_count) "
        "VALUES (1, 1, 'plex://movie/abc', 8.0, 5)");

    /* Rust translator treats metadata_item_settings with composite conflict */
    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(account_id, guid)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                !contains_ci(r.sql, "OR REPLACE"),
                r, "Expected metadata_item_settings ON CONFLICT(account_id, guid)");
    sql_translation_free(&r);
}

static void test_schema_migrations(void) {
    TEST("schema_migrations → ON CONFLICT(version)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO schema_migrations (version) VALUES ('20240101')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(version)") &&
                !contains_ci(r.sql, "OR REPLACE"),
                r, "Expected schema_migrations conflict on version");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* INSERT OR IGNORE → DO NOTHING                                              */
/* ========================================================================== */

static void test_insert_or_ignore(void) {
    printf("\n\033[1mINSERT OR IGNORE:\033[0m\n");

    TEST("INSERT OR IGNORE → ON CONFLICT DO NOTHING");
    sql_translation_t r = sql_translate(
        "INSERT OR IGNORE INTO tags (id, tag) VALUES (1, 'test')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT") &&
                contains_ci(r.sql, "DO NOTHING") &&
                !contains_ci(r.sql, "OR IGNORE"),
                r, "Expected DO NOTHING");
    sql_translation_free(&r);
}

static void test_insert_or_ignore_unknown_table(void) {
    TEST("INSERT OR IGNORE unknown table → ON CONFLICT DO NOTHING");
    sql_translation_t r = sql_translate(
        "INSERT OR IGNORE INTO unknown_tbl (id, data) VALUES (1, 'test')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "DO NOTHING") &&
                !contains_ci(r.sql, "OR IGNORE"),
                r, "Expected DO NOTHING for unknown table");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* REPLACE INTO → same as INSERT OR REPLACE                                   */
/* ========================================================================== */

static void test_replace_into(void) {
    printf("\n\033[1mREPLACE INTO:\033[0m\n");

    TEST("REPLACE INTO → INSERT INTO ... ON CONFLICT DO UPDATE");
    sql_translation_t r = sql_translate(
        "REPLACE INTO tags (id, tag) VALUES (1, 'test')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "INSERT INTO") &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                !contains_ci(r.sql, "REPLACE INTO"),
                r, "Expected REPLACE INTO rewritten");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* Schema prefix handling                                                     */
/* ========================================================================== */

static void test_schema_prefix(void) {
    printf("\n\033[1mSchema prefix handling:\033[0m\n");

    TEST("plex.tags → resolved to tags, ON CONFLICT(id)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO plex.tags (id, tag, tag_type) VALUES (1, 'Action', 0)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected plex.tags resolved correctly");
    sql_translation_free(&r);
}

static void test_schema_prefix_composite(void) {
    TEST("plex.preferences → resolved, ON CONFLICT(name)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO plex.preferences (id, name, value) VALUES (1, 'key', 'val')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(name)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected plex.preferences resolved");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* SET clause: id and conflict columns excluded                               */
/* ========================================================================== */

static void test_regular_column_excluded(void) {
    printf("\n\033[1mSET clause column handling:\033[0m\n");

    TEST("Regular columns → col = EXCLUDED.col");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags "
        "(id, tag, tag_type) "
        "VALUES (1, 'Action', 0)");

    test_assert(r.success && r.sql &&
                contains_exact(r.sql, "tag = EXCLUDED.tag") &&
                contains_exact(r.sql, "tag_type = EXCLUDED.tag_type"),
                r, "Expected col = EXCLUDED.col");
    sql_translation_free(&r);
}

static void test_id_column_skipped_in_set(void) {
    TEST("id column → skipped in SET clause");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags "
        "(id, tag) "
        "VALUES (1, 'Action')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                !contains_exact(r.sql, "id = EXCLUDED.id"),
                r, "Expected id excluded from SET");
    sql_translation_free(&r);
}

static void test_conflict_columns_skipped_in_set(void) {
    TEST("Composite conflict cols → skipped in SET clause");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO statistics_bandwidth "
        "(id, account_id, device_id, timespan, at, lan, bytes) "
        "VALUES (1, 2, 3, 4, 5, 1, 1024)");

    /* account_id, device_id, timespan, at, lan are conflict cols → skipped */
    /* bytes should be in SET clause */
    test_assert(r.success && r.sql &&
                contains_exact(r.sql, "bytes = EXCLUDED.bytes") &&
                !contains_exact(r.sql, "account_id = EXCLUDED.account_id") &&
                !contains_exact(r.sql, "device_id = EXCLUDED.device_id") &&
                !contains_exact(r.sql, "timespan = EXCLUDED.timespan") &&
                !contains_exact(r.sql, "lan = EXCLUDED.lan"),
                r, "Expected only non-conflict cols in SET");
    sql_translation_free(&r);
}

static void test_metadata_item_settings_set_clause(void) {
    TEST("metadata_item_settings → account_id, guid skipped; rating, view_count in SET");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO metadata_item_settings "
        "(id, account_id, guid, rating, view_count) "
        "VALUES (1, 1, 'guid', 5.0, 10)");

    test_assert(r.success && r.sql &&
                contains_exact(r.sql, "rating = EXCLUDED.rating") &&
                contains_exact(r.sql, "view_count = EXCLUDED.view_count") &&
                !contains_exact(r.sql, "account_id = EXCLUDED.account_id") &&
                !contains_exact(r.sql, "guid = EXCLUDED.guid") &&
                !contains_exact(r.sql, "id = EXCLUDED.id"),
                r, "Expected only non-conflict cols in SET");
    sql_translation_free(&r);
}

static void test_preferences_set_clause(void) {
    TEST("preferences → name skipped, id skipped, value in SET");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO preferences "
        "(id, name, value) "
        "VALUES (1, 'key', 'val')");

    test_assert(r.success && r.sql &&
                contains_exact(r.sql, "value = EXCLUDED.value") &&
                !contains_exact(r.sql, "name = EXCLUDED.name") &&
                !contains_exact(r.sql, "id = EXCLUDED.id"),
                r, "Expected only value in SET");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* RETURNING id                                                               */
/* ========================================================================== */

static void test_returning_id_for_id_conflict(void) {
    printf("\n\033[1mRETURNING id:\033[0m\n");

    TEST("(id) conflict → appends RETURNING id");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags (id, tag) VALUES (1, 'Action')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "RETURNING id"),
                r, "Expected RETURNING id");
    sql_translation_free(&r);
}

static void test_no_returning_for_name_conflict(void) {
    TEST("(name) conflict → no RETURNING id");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO preferences (id, name, value) VALUES (1, 'key', 'val')");

    /* preferences has ON CONFLICT(name) — "name" doesn't contain "id" → no RETURNING */
    test_assert(r.success && r.sql &&
                !contains_ci(r.sql, "RETURNING"),
                r, "Expected no RETURNING for name conflict");
    sql_translation_free(&r);
}

static void test_no_returning_for_version_conflict(void) {
    TEST("(version) conflict → no RETURNING id");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO schema_migrations (version) VALUES ('20240101')");

    /* "version" doesn't contain "id" → no RETURNING */
    test_assert(r.success && r.sql &&
                !contains_ci(r.sql, "RETURNING"),
                r, "Expected no RETURNING for version conflict");
    sql_translation_free(&r);
}

static void test_returning_for_composite_with_id(void) {
    TEST("Composite conflict with account_id → has RETURNING id");
    /* statistics_bandwidth conflict cols: account_id, device_id, ... */
    /* "account_id" contains "id" as substring → RETURNING id */
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO statistics_bandwidth "
        "(id, account_id, device_id, timespan, at, lan, bytes) "
        "VALUES (1, 2, 3, 4, 5, 1, 1024)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "RETURNING id"),
                r, "Expected RETURNING id (account_id contains 'id')");
    sql_translation_free(&r);
}

static void test_returning_for_metadata_item_settings(void) {
    TEST("metadata_item_settings (account_id, guid) → has RETURNING id");
    /* "account_id" contains "id" → RETURNING id */
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO metadata_item_settings "
        "(id, account_id, guid, rating) VALUES (1, 1, 'guid', 5.0)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "RETURNING id"),
                r, "Expected RETURNING id (account_id contains 'id')");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* Unknown table → fallback (no conflict target specified)                    */
/* ========================================================================== */

static void test_unknown_table_default(void) {
    printf("\n\033[1mDefault fallback for unknown tables:\033[0m\n");

    TEST("unknown_table → ON CONFLICT DO UPDATE SET (no target cols)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO some_unknown_table (id, data) VALUES (1, 'test')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                !contains_ci(r.sql, "OR REPLACE"),
                r, "Expected ON CONFLICT for unknown table");
    sql_translation_free(&r);
}

static void test_unknown_table_set_clause(void) {
    TEST("unknown_table → SET skips id, includes others");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO some_unknown_table (id, data, value) VALUES (1, 'test', 42)");

    test_assert(r.success && r.sql &&
                contains_exact(r.sql, "data = EXCLUDED.data") &&
                contains_exact(r.sql, "value = EXCLUDED.value") &&
                !contains_exact(r.sql, "id = EXCLUDED.id"),
                r, "Expected id excluded from SET for unknown table");
    sql_translation_free(&r);
}

static void test_unknown_table_no_returning(void) {
    TEST("unknown_table → no RETURNING id (no conflict target)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO some_unknown_table (id, data) VALUES (1, 'test')");

    /* Unknown tables have no conflict_cols → should_add_returning_id returns false */
    test_assert(r.success && r.sql &&
                !contains_ci(r.sql, "RETURNING"),
                r, "Expected no RETURNING for unknown table");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* Case insensitivity                                                         */
/* ========================================================================== */

static void test_case_insensitive_keyword(void) {
    printf("\n\033[1mCase handling:\033[0m\n");

    TEST("insert or replace INTO → works (mixed case)");
    sql_translation_t r = sql_translate(
        "insert or replace INTO tags (id, tag) VALUES (1, 'Action')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                !contains_ci(r.sql, "or replace"),
                r, "Expected mixed case handled");
    sql_translation_free(&r);
}

static void test_case_insensitive_table(void) {
    TEST("METADATA_ITEMS (uppercase table) → resolved correctly");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO METADATA_ITEMS (id, title) VALUES (1, 'Test')");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected uppercase table resolved");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* Quoted column names                                                        */
/* ========================================================================== */

static void test_quoted_columns(void) {
    printf("\n\033[1mQuoted column names:\033[0m\n");

    TEST("Quoted columns → parsed correctly, ON CONFLICT(id)");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags "
        "(\"id\", \"tag\", \"tag_type\") "
        "VALUES (1, 'Action', 0)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                /* Quoted columns keep quotes in EXCLUDED refs */
                contains_ci(r.sql, "EXCLUDED"),
                r, "Expected quoted columns handled");
    sql_translation_free(&r);
}

static void test_mixed_quoted_columns(void) {
    TEST("Mixed quoted/unquoted columns → all parsed");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags "
        "(\"id\", tag, \"tag_type\", created_at) "
        "VALUES (1, 'Action', 0, 12345)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "EXCLUDED.tag") &&
                contains_ci(r.sql, "EXCLUDED") &&
                contains_ci(r.sql, "created_at"),
                r, "Expected mixed quoted columns handled");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* Trailing semicolons and whitespace                                         */
/* ========================================================================== */

static void test_trailing_semicolon(void) {
    printf("\n\033[1mTrailing content handling:\033[0m\n");

    TEST("Trailing semicolon → handled cleanly");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags (id, tag) VALUES (1, 'Action');");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected semicolon handled");
    sql_translation_free(&r);
}

static void test_trailing_whitespace(void) {
    TEST("Trailing whitespace → handled cleanly");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags (id, tag) VALUES (1, 'Action')   ");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET"),
                r, "Expected whitespace handled");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* Realistic Plex SQL patterns                                                */
/* ========================================================================== */

static void test_real_plex_media_items(void) {
    printf("\n\033[1mRealistic Plex SQL patterns:\033[0m\n");

    TEST("Real media_items INSERT → ON CONFLICT(id) DO UPDATE ... RETURNING id");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO media_items "
        "(id, library_section_id, section_location_id, metadata_item_id, "
        "media_type, width, height, size, duration, bitrate, "
        "container, video_codec, audio_codec, display_aspect_ratio, "
        "frames_per_second, audio_channels, created_at, updated_at) "
        "VALUES (100, 1, 1, 50, 1, 1920, 1080, 5000000, 7200, 5000, "
        "'mkv', 'h264', 'aac', 1.78, 23.976, 2, 1234567890, 1234567890)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                !contains_exact(r.sql, "id = EXCLUDED.id") &&
                contains_ci(r.sql, "RETURNING id") &&
                contains_exact(r.sql, "updated_at = EXCLUDED.updated_at"),
                r, "Expected full media_items translation");
    sql_translation_free(&r);
}

static void test_real_plex_statistics_bandwidth(void) {
    TEST("Real statistics_bandwidth INSERT → composite conflict");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO statistics_bandwidth "
        "(id, account_id, device_id, timespan, at, lan, bytes, created_at, updated_at) "
        "VALUES (500, 1, 42, 3600, 1234567890, 1, 1048576, 1234567890, 1234567890)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(account_id, device_id, timespan, at, lan)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                contains_exact(r.sql, "bytes = EXCLUDED.bytes") &&
                contains_exact(r.sql, "updated_at = EXCLUDED.updated_at") &&
                contains_ci(r.sql, "RETURNING id"),
                r, "Expected bandwidth composite conflict");
    sql_translation_free(&r);
}

static void test_real_plex_metadata_items(void) {
    TEST("Real metadata_items INSERT → large column set");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO metadata_items "
        "(id, metadata_type, title, title_sort, original_title, "
        "studio, rating, summary, tagline, year, "
        "library_section_id, created_at, updated_at, changed_at) "
        "VALUES (1, 1, 'Test Movie', 'test movie', 'Original', "
        "'Studio', 8.5, 'A summary', 'A tagline', 2024, "
        "1, 1234567890, 1234567890, 1234567890)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(id)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                contains_exact(r.sql, "title = EXCLUDED.title") &&
                contains_exact(r.sql, "updated_at = EXCLUDED.updated_at") &&
                contains_exact(r.sql, "changed_at = EXCLUDED.changed_at") &&
                !contains_exact(r.sql, "id = EXCLUDED.id") &&
                contains_ci(r.sql, "RETURNING id"),
                r, "Expected large metadata_items translation");
    sql_translation_free(&r);
}

static void test_real_plex_metadata_item_settings(void) {
    TEST("Real metadata_item_settings → ON CONFLICT(account_id, guid) RETURNING id");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO metadata_item_settings "
        "(id, account_id, guid, rating, view_count) "
        "VALUES (1, 1, 'plex://movie/abc', 8.0, 5)");

    test_assert(r.success && r.sql &&
                contains_ci(r.sql, "ON CONFLICT(account_id, guid)") &&
                contains_ci(r.sql, "DO UPDATE SET") &&
                contains_exact(r.sql, "rating = EXCLUDED.rating") &&
                contains_exact(r.sql, "view_count = EXCLUDED.view_count") &&
                !contains_exact(r.sql, "account_id = EXCLUDED.account_id") &&
                !contains_exact(r.sql, "guid = EXCLUDED.guid") &&
                contains_ci(r.sql, "RETURNING id"),
                r, "Expected metadata_item_settings translation");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* OR REPLACE fully stripped                                                   */
/* ========================================================================== */

static void test_or_replace_stripped(void) {
    printf("\n\033[1mOR REPLACE removal:\033[0m\n");

    TEST("INSERT OR REPLACE → OR REPLACE fully removed from output");
    sql_translation_t r = sql_translate(
        "INSERT OR REPLACE INTO tags (id, tag) VALUES (1, 'test')");

    test_assert(r.success && r.sql &&
                !contains_ci(r.sql, "OR REPLACE") &&
                contains_ci(r.sql, "INSERT INTO"),
                r, "Expected OR REPLACE removed");
    sql_translation_free(&r);
}

static void test_or_ignore_stripped(void) {
    TEST("INSERT OR IGNORE → OR IGNORE fully removed from output");
    sql_translation_t r = sql_translate(
        "INSERT OR IGNORE INTO tags (id, tag) VALUES (1, 'test')");

    test_assert(r.success && r.sql &&
                !contains_ci(r.sql, "OR IGNORE") &&
                contains_ci(r.sql, "INSERT INTO"),
                r, "Expected OR IGNORE removed");
    sql_translation_free(&r);
}

static void test_replace_into_stripped(void) {
    TEST("REPLACE INTO → REPLACE keyword removed from output");
    sql_translation_t r = sql_translate(
        "REPLACE INTO tags (id, tag) VALUES (1, 'test')");

    test_assert(r.success && r.sql &&
                !contains_ci(r.sql, "REPLACE INTO") &&
                contains_ci(r.sql, "INSERT INTO"),
                r, "Expected REPLACE INTO rewritten to INSERT INTO");
    sql_translation_free(&r);
}

/* ========================================================================== */
/* Main                                                                       */
/* ========================================================================== */

int main(void) {
    printf("==========================================================\n");
    printf("INSERT OR REPLACE → ON CONFLICT Translation Tests\n");
    printf("(Rust-only sql_translate() backend)\n");
    printf("==========================================================\n");

    sql_translator_init();

    /* Edge cases */
    printf("\n\033[1mEdge cases:\033[0m\n");
    test_null_input();
    test_non_insert();
    test_plain_insert();
    test_no_column_list();

    /* All id-based tables */
    test_all_id_tables();

    /* Composite/unique conflict targets */
    test_statistics_bandwidth();
    test_locatables();
    test_location_places();
    test_media_stream_settings();
    test_preferences();
    test_metadata_item_settings();
    test_schema_migrations();

    /* INSERT OR IGNORE */
    test_insert_or_ignore();
    test_insert_or_ignore_unknown_table();

    /* REPLACE INTO */
    test_replace_into();

    /* Schema prefix */
    test_schema_prefix();
    test_schema_prefix_composite();

    /* SET clause column handling */
    test_regular_column_excluded();
    test_id_column_skipped_in_set();
    test_conflict_columns_skipped_in_set();
    test_metadata_item_settings_set_clause();
    test_preferences_set_clause();

    /* RETURNING id */
    test_returning_id_for_id_conflict();
    test_no_returning_for_name_conflict();
    test_no_returning_for_version_conflict();
    test_returning_for_composite_with_id();
    test_returning_for_metadata_item_settings();

    /* Unknown table fallback */
    test_unknown_table_default();
    test_unknown_table_set_clause();
    test_unknown_table_no_returning();

    /* Case handling */
    test_case_insensitive_keyword();
    test_case_insensitive_table();

    /* Quoted columns */
    test_quoted_columns();
    test_mixed_quoted_columns();

    /* Trailing content */
    test_trailing_semicolon();
    test_trailing_whitespace();

    /* Realistic Plex SQL */
    test_real_plex_media_items();
    test_real_plex_statistics_bandwidth();
    test_real_plex_metadata_items();
    test_real_plex_metadata_item_settings();

    /* OR REPLACE / OR IGNORE removal */
    test_or_replace_stripped();
    test_or_ignore_stripped();
    test_replace_into_stripped();

    sql_translator_cleanup();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);

    return tests_failed > 0 ? 1 : 0;
}
