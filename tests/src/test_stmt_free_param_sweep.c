#include <assert.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>

#include "../../src/pg_statement.h"

// Stubs to satisfy pg_statement.o link dependencies.
void pg_log_message_internal(int level, const char *fmt, ...) {
    (void)level;
    (void)fmt;
}

void pg_query_cache_release(cached_result_t *entry) {
    (void)entry;
}

static void *tracked_a;
static void *tracked_b;

int main(void) {
    pg_stmt_t *stmt = pg_stmt_create(NULL, "SELECT 1", NULL);
    assert(stmt != NULL);

    stmt->param_count = 1;

    tracked_a = malloc(16);
    tracked_b = malloc(1024 * 1024);
    assert(tracked_a != NULL);
    assert(tracked_b != NULL);

    stmt->param_values[0] = (char *)tracked_a;
    stmt->param_values[200] = (char *)tracked_b;

    // pg_stmt_free expects ref_count to be 0 at destruction point.
    atomic_store(&stmt->ref_count, 0);
    pg_stmt_free(stmt);

    // The Makefile runs this binary under `leaks --atExit`.
    // If slot 200 is not freed, that check will fail.
    printf("PASS: pg_stmt_free sweep executed\n");
    return 0;
}
