/*
 * Unit tests for rewrite_server_library_uri()
 *
 * Tests the server:// -> library:// URI rewriting used to fix
 * "LPE: only library URIs are allowed right now" errors.
 *
 * The function is static in db_interpose_column.c, so we duplicate it here.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int passed = 0;
static int failed = 0;

#define BOLD  "\033[1m"
#define GREEN "\033[32m"
#define RED   "\033[31m"
#define RESET "\033[0m"

#define TEST(name) printf("  Testing: %-60s ", name)
#define PASS() do { printf(GREEN "PASS" RESET "\n"); passed++; } while(0)
#define FAIL(msg) do { printf(RED "FAIL: %s" RESET "\n", msg); failed++; } while(0)

/* Stub - not needed for this test */
#define LOG_DEBUG(...) do {} while(0)
#define LOG_INFO(...)  do {} while(0)

/* ---- copy of rewrite_server_library_uri from db_interpose_column.c ---- */

static int rewrite_server_library_uri(const char *in, char *out, size_t out_len) {
    if (!in || !out || out_len < 16) return 0;

    static const char server_prefix[] = "server://";
    static const size_t server_prefix_len = sizeof(server_prefix) - 1;
    static const char needle[] = "/com.plexapp.plugins.library/library/";
    static const size_t needle_len = sizeof(needle) - 1;
    static const char replacement[] = "library://";
    static const size_t replacement_len = sizeof(replacement) - 1;

    if (!strstr(in, server_prefix)) return 0;
    if (!strstr(in, needle)) return 0;

    size_t in_pos = 0;
    size_t out_pos = 0;
    size_t in_len = strlen(in);
    int rewrites = 0;

    while (in_pos < in_len) {
        const char *match = strstr(in + in_pos, server_prefix);
        if (!match) {
            size_t remaining = in_len - in_pos;
            if (out_pos + remaining >= out_len) remaining = out_len - out_pos - 1;
            memcpy(out + out_pos, in + in_pos, remaining);
            out_pos += remaining;
            break;
        }

        size_t prefix_bytes = (size_t)(match - (in + in_pos));
        if (out_pos + prefix_bytes >= out_len) {
            size_t fits = out_len - out_pos - 1;
            if (fits > 0) memcpy(out + out_pos, in + in_pos, fits);
            out_pos += fits;
            break;
        }
        memcpy(out + out_pos, in + in_pos, prefix_bytes);
        out_pos += prefix_bytes;
        in_pos += prefix_bytes;

        const char *needle_pos = strstr(in + in_pos + server_prefix_len, needle);
        if (!needle_pos) {
            size_t copy = server_prefix_len;
            if (out_pos + copy >= out_len) copy = out_len - out_pos - 1;
            memcpy(out + out_pos, in + in_pos, copy);
            out_pos += copy;
            in_pos += server_prefix_len;
            continue;
        }

        size_t full_prefix_len = (size_t)(needle_pos - (in + in_pos)) + needle_len;
        if (out_pos + replacement_len >= out_len) break;
        memcpy(out + out_pos, replacement, replacement_len);
        out_pos += replacement_len;
        in_pos += full_prefix_len;
        rewrites++;
    }

    out[out_pos] = '\0';
    return rewrites > 0 ? 1 : 0;
}

/* ---- tests ---- */

static void test_standalone_uri(void) {
    TEST("standalone server:// URI");
    char out[512];
    const char *in = "server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all?type=2";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (ok && strcmp(out, "library://sections/2/all?type=2") == 0) PASS();
    else FAIL(out);
}

static void test_json_embedded_uri(void) {
    TEST("JSON-embedded pv:uri");
    char out[1024];
    const char *in = "{\"at:childCount\":\"1\",\"at:smart\":\"1\",\"pv:uri\":\"server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=date\"}";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (!ok) { FAIL("returned 0"); return; }
    const char *expect = "{\"at:childCount\":\"1\",\"at:smart\":\"1\",\"pv:uri\":\"library://sections/2/all?type=2&sort=date\"}";
    if (strcmp(out, expect) == 0) PASS();
    else { printf("\n    got:    %s\n    expect: %s\n", out, expect); FAIL("mismatch"); }
}

static void test_no_server_prefix(void) {
    TEST("no server:// -> no rewrite");
    char out[256];
    const char *in = "library://sections/1/all";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (!ok) PASS();
    else FAIL("should not rewrite");
}

static void test_server_without_plugin_path(void) {
    TEST("server:// without plugin path -> no rewrite");
    char out[256];
    const char *in = "server://abc123/some/other/path";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (!ok) PASS();
    else FAIL("should not rewrite");
}

static void test_empty_string(void) {
    TEST("empty string -> no rewrite");
    char out[64];
    int ok = rewrite_server_library_uri("", out, sizeof(out));
    if (!ok) PASS();
    else FAIL("should not rewrite empty");
}

static void test_null_input(void) {
    TEST("NULL input -> no rewrite");
    char out[64];
    int ok = rewrite_server_library_uri(NULL, out, sizeof(out));
    if (!ok) PASS();
    else FAIL("should not rewrite NULL");
}

static void test_plain_text(void) {
    TEST("plain text without URIs -> no rewrite");
    char out[256];
    const char *in = "{\"at:childCount\":\"5\",\"pv:thumbBlurHash\":\"abc123\"}";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (!ok) PASS();
    else FAIL("should not rewrite plain JSON");
}

static void test_multiple_uris_in_json(void) {
    TEST("multiple server:// URIs in one string");
    char out[2048];
    const char *in = "{\"uri1\":\"server://aaa/com.plexapp.plugins.library/library/sections/1/all\","
                     "\"uri2\":\"server://aaa/com.plexapp.plugins.library/library/sections/2/all\"}";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (!ok) { FAIL("returned 0"); return; }
    const char *expect = "{\"uri1\":\"library://sections/1/all\","
                         "\"uri2\":\"library://sections/2/all\"}";
    if (strcmp(out, expect) == 0) PASS();
    else { printf("\n    got:    %s\n    expect: %s\n", out, expect); FAIL("mismatch"); }
}

static void test_output_shorter_than_input(void) {
    TEST("output is shorter than input");
    char out[512];
    const char *in = "server://71b2873061a562bf7541852f9a43087e88a63f9a/com.plexapp.plugins.library/library/sections/2/all";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (!ok) { FAIL("returned 0"); return; }
    if (strlen(out) < strlen(in)) PASS();
    else FAIL("output should be shorter");
}

static void test_uri_with_encoded_params(void) {
    TEST("URI with URL-encoded query params");
    char out[1024];
    const char *in = "server://71b287/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=originallyAvailableAt%3Adesc&push=1&show.network=248684&pop=1";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (!ok) { FAIL("returned 0"); return; }
    if (strcmp(out, "library://sections/2/all?type=2&sort=originallyAvailableAt%3Adesc&push=1&show.network=248684&pop=1") == 0) PASS();
    else FAIL(out);
}

static void test_small_buffer(void) {
    TEST("small output buffer -> truncated but safe");
    char out[32];
    const char *in = "server://abc/com.plexapp.plugins.library/library/sections/2/all?type=2&sort=date";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    /* Should still rewrite (or at least not crash) */
    if (ok && strlen(out) < sizeof(out)) PASS();
    else if (!ok) PASS(); /* buffer too small to fit replacement is also acceptable */
    else FAIL("unexpected result");
}

static void test_tiny_buffer(void) {
    TEST("buffer < 16 -> returns 0");
    char out[8];
    int ok = rewrite_server_library_uri("server://x", out, sizeof(out));
    if (!ok) PASS();
    else FAIL("should refuse tiny buffer");
}

static void test_real_plex_extra_data(void) {
    TEST("real Plex extra_data JSON blob");
    char out[2048];
    const char *in = "{\"at:childCount\":\"1\",\"at:smart\":\"1\","
        "\"pv:blurHashesChangedAt\":\"277470\","
        "\"pv:thumbBlurHash\":\"LJC?YqM{IVoz\","
        "\"pv:uri\":\"server://71b2873061a562bf7541852f9a43087e88a63f9a"
        "/com.plexapp.plugins.library/library/sections/2/all"
        "?type=2&sort=originallyAvailableAt%3Adesc&push=1&show.genre=8966&pop=1\"}";
    int ok = rewrite_server_library_uri(in, out, sizeof(out));
    if (!ok) { FAIL("returned 0"); return; }
    /* Verify library:// is in output and server:// is not */
    if (strstr(out, "library://sections/2/all") && !strstr(out, "server://")) PASS();
    else { printf("\n    got: %.200s\n", out); FAIL("rewrite incomplete"); }
}

int main(void) {
    printf(BOLD "\nURI Rewrite Tests (server:// -> library://):\n" RESET);

    test_standalone_uri();
    test_json_embedded_uri();
    test_no_server_prefix();
    test_server_without_plugin_path();
    test_empty_string();
    test_null_input();
    test_plain_text();
    test_multiple_uris_in_json();
    test_output_shorter_than_input();
    test_uri_with_encoded_params();
    test_small_buffer();
    test_tiny_buffer();
    test_real_plex_extra_data();

    printf("\n" BOLD "=== Results ===" RESET "\n");
    printf("Passed: " GREEN "%d" RESET "\n", passed);
    printf("Failed: " RED "%d" RESET "\n", failed);

    return failed > 0 ? 1 : 0;
}
