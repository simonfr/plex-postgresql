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

int main(void) {
    pg_stmt_t *stmt = pg_stmt_create(NULL, "SELECT ?", NULL);
    assert(stmt != NULL);

    // Simulate translator saying 1 parameter, while bind mapping accidentally
    // stores captured values into higher slots.
    stmt->param_count = 1;

    for (int i = 1; i < MAX_PARAMS; i++) {
        char *buf = (char *)malloc(256);
        assert(buf != NULL);
        buf[0] = 'x';
        buf[1] = '\0';
        stmt->param_values[i] = buf;
    }

    // Free through the production refcount path.
    atomic_store(&stmt->ref_count, 1);
    pg_stmt_unref(stmt);

    // Run under `leaks --atExit` from Makefile. Any slot missed by cleanup
    // will show up there.
    printf("PASS: bind-index mismatch cleanup path executed\n");
    return 0;
}
