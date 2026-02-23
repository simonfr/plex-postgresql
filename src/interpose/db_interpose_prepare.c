/*
 * Plex PostgreSQL Interposing Shim - Prepare Operations
 *
 * Handles sqlite3_prepare*, including recursion prevention and stack protection.
 */

// Must be defined before any includes for pthread_getattr_np on Linux/musl
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include "db_interpose.h"
#include "db_interpose_common.h"
#include "db_interpose_rust.h"
#include <time.h>
#include <sys/time.h>
#include "shim_alloc.h"

static char *maybe_alias_collection_sync_aggregates(const char *sqlite_sql, const char *pg_sql);

// ---------------------------------------------------------------------------
// Targeted SQL prepare tracing (low volume)
//
// Logs SQL strings at prepare time, before any row/column access happens.
// This is useful when Plex throws std::bad_cast outside of sqlite3_column_* paths.
//
// Configure via:
//   PLEX_PG_TRACE_PREPARE_SQL_CONTAINS="tags,taggings,collections"
// or:
//   /tmp/plex_pg_trace_prepare_sql_contains
// ---------------------------------------------------------------------------

static int trace_prepare_sql_ok(const char *sql) {
    return rust_trace_prepare_sql_ok(sql);
}

static void trace_prepare_pgsql_if_enabled(const char *sqlite_sql, const char *pg_sql) {
    if (!trace_prepare_sql_ok(sqlite_sql)) return;
    if (!pg_sql) return;
    LOG_ERROR("TRACE_PREPARE_PGSQL: %.900s", pg_sql);
}

static unsigned long g_txn_route_total = 0;
static unsigned long g_txn_route_skipped = 0;
static unsigned long g_txn_route_pg = 0;

static int is_txn_control_sql(const char *sql) {
    if (!sql) return 0;
    while (*sql == ' ' || *sql == '\t' || *sql == '\n' || *sql == '\r') sql++;
    return strncasecmp(sql, "begin", 5) == 0 ||
           strncasecmp(sql, "commit", 6) == 0 ||
           strncasecmp(sql, "rollback", 8) == 0 ||
           strncasecmp(sql, "savepoint", 9) == 0 ||
           strncasecmp(sql, "release savepoint", 17) == 0;
}

// ============================================================================
// Query Loop Detection
// ============================================================================
// Plex can get into infinite query loops (e.g., OnDeck with many views).
// We detect this by tracking recent query hashes and breaking the loop.

// Returns 1 if query loop detected and should be broken
static int detect_query_loop(const char *sql) {
    if (!sql) return 0;

    int count = 0;
    uint64_t elapsed_ms = 0;
    if (rust_prepare_query_loop_tick(sql, &count, &elapsed_ms)) {
        // Only log every 10th detection to reduce spam
        static __thread int log_counter = 0;
        if (log_counter++ % 10 == 0) {
            LOG_INFO("High-frequency query: %d calls in %llu ms (likely batch operation with different params) sql=%.200s",
                     count, (unsigned long long)elapsed_ms, sql);
        }
    }

    // Don't break - Plex crashes on empty results
    // The prepared statement caching makes the queries fast anyway
    return 0;
}

// ============================================================================
// Helper Functions
// ============================================================================

// Helper to create a simplified SQL for SQLite when query uses FTS
// Removes FTS joins and MATCH clauses since SQLite shadow DB doesn't have FTS tables
char* simplify_fts_for_sqlite(const char *sql) {
    char *r = rust_simplify_fts_for_sqlite(sql);
    if (!r) return NULL;
    char *out = strdup(r);
    rust_free_cstring(r);
    return out;
}

static char *strip_collate_icu_root_for_sqlite(const char *sql) {
    char *r = rust_strip_collate_icu_root(sql);
    if (!r) return NULL;
    char *out = strdup(r);
    rust_free_cstring(r);
    return out;
}

static char *add_if_not_exists_for_sqlite_ddl(const char *sql) {
    char *r = rust_add_if_not_exists_for_sqlite_ddl(sql);
    if (!r) return NULL;
    char *out = strdup(r);
    rust_free_cstring(r);
    return out;
}

// ============================================================================
// Internal Prepare Implementation
// ============================================================================

// Helper: Check if column exists in SQLite table
static int column_exists_in_sqlite(sqlite3 *db, const char *table_name, const char *column_name) {
    if (!db || !table_name || !column_name || !shim_sqlite3_prepare_v2) return 0;

    // Query SQLite's table_info pragma to check if column exists
    char pragma_sql[512];
    snprintf(pragma_sql, sizeof(pragma_sql), "PRAGMA table_info(%s)", table_name);

    sqlite3_stmt *stmt = NULL;
    int rc = shim_sqlite3_prepare_v2(db, pragma_sql, -1, &stmt, NULL);
    if (rc != SQLITE_OK || !stmt) return 0;

    int found = 0;
    while (orig_sqlite3_step && orig_sqlite3_step(stmt) == SQLITE_ROW) {
        // Column 1 is the column name
        const char *col = (const char *)orig_sqlite3_column_text(stmt, 1);
        if (col && strcasecmp(col, column_name) == 0) {
            found = 1;
            break;
        }
    }

    if (orig_sqlite3_finalize) orig_sqlite3_finalize(stmt);
    return found;
}

// Internal prepare_v2 implementation - called either directly or from worker thread
// from_worker: 0 = called from Plex's thread, 1 = called from worker with large stack
int my_sqlite3_prepare_v2_internal(sqlite3 *db, const char *zSql, int nByte,
                                   sqlite3_stmt **ppStmt, const char **pzTail,
                                   int from_worker) {
    pg_exception_note_phase("prepare_v2", zSql, NULL, db);
    if (zSql) {
        pg_exception_note_query(zSql);
    }

    if (trace_prepare_sql_ok(zSql)) {
        LOG_DEBUG("TRACE_PREPARE_SQL: %.700s", zSql);
    }

    // SyncCollections: previously skipped with empty results to avoid std::bad_cast.
    // Now letting them through — the root causes are fixed:
    //   - dt_integer(8) decltype for OID=20 (bigint) columns
    //   - Column alias fix for count(*)/min(year)/max(year)
    // Skipping these caused 200+ "Failed to generate a query" LPE errors per startup
    // because Plex had no collection data to build hub queries from.
    // HANDLE ALTER TABLE ADD COLUMN: Skip if column already exists
    // This prevents "duplicate column name" errors when Plex reruns migrations
    if (zSql && strcasestr(zSql, "ALTER TABLE") && strcasestr(zSql, " ADD ")) {
        // Parse: ALTER TABLE 'table_name' ADD 'column_name' type
        // or:    ALTER TABLE "table_name" ADD "column_name" type
        const char *table_start = strcasestr(zSql, "ALTER TABLE");
        if (table_start) {
            table_start += 11; // Skip "ALTER TABLE"
            while (*table_start == ' ') table_start++;

            // Extract table name (may be quoted with ' or ")
            char table_name[256] = {0};
            char quote = 0;
            if (*table_start == '\'' || *table_start == '"') {
                quote = *table_start++;
                const char *end = strchr(table_start, quote);
                if (end && (end - table_start) < 255) {
                    strncpy(table_name, table_start, end - table_start);
                }
            } else {
                // Unquoted table name
                int i = 0;
                while (table_start[i] && table_start[i] != ' ' && i < 255) {
                    table_name[i] = table_start[i];
                    i++;
                }
            }

            // Find ADD and extract column name
            const char *add_pos = strcasestr(zSql, " ADD ");
            if (add_pos && table_name[0]) {
                add_pos += 5; // Skip " ADD "
                while (*add_pos == ' ') add_pos++;

                char column_name[256] = {0};
                if (*add_pos == '\'' || *add_pos == '"') {
                    quote = *add_pos++;
                    const char *end = strchr(add_pos, quote);
                    if (end && (end - add_pos) < 255) {
                        strncpy(column_name, add_pos, end - add_pos);
                    }
                } else {
                    int i = 0;
                    while (add_pos[i] && add_pos[i] != ' ' && i < 255) {
                        column_name[i] = add_pos[i];
                        i++;
                    }
                }

                // Check if column already exists
                if (column_name[0] && column_exists_in_sqlite(db, table_name, column_name)) {
                    LOG_INFO("ALTER TABLE ADD COLUMN skipped (column '%s' already exists in '%s')",
                             column_name, table_name);
                    // Return a dummy statement that does nothing
                    if (shim_sqlite3_prepare_v2) {
                        int rc = shim_sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, ppStmt, pzTail);
                        return rc;
                    }
                    if (ppStmt) *ppStmt = NULL;
                    if (pzTail) *pzTail = NULL;
                    return SQLITE_OK;
                }
            }
        }
    }

    // LOOP DETECTION: Break infinite query loops (e.g., OnDeck with many views)
    if (zSql && detect_query_loop(zSql)) {
        // Return empty result to break the loop
        if (shim_sqlite3_prepare_v2) {
            int rc = shim_sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, ppStmt, pzTail);
            return rc;
        }
        if (ppStmt) *ppStmt = NULL;
        if (pzTail) *pzTail = NULL;
        return SQLITE_OK;
    }

    // CRITICAL: Track recursion depth to prevent infinite loops
    // SQLite can internally call prepare_v2 again, creating deep recursion
    prepare_v2_depth++;

    // If recursion is too deep, bail out immediately to prevent stack overflow
    // Normal operations should never recurse more than 5-10 times
    // The crash on 2026-01-06 showed 218 recursive frames!
    // Reduced from 100 to 50: 50 levels × 2KB = 100KB reserved for recursion
    if (prepare_v2_depth > 50) {
        LOG_ERROR("RECURSION LIMIT: prepare_v2 called %d times (depth=%d)!",
                  prepare_v2_depth, prepare_v2_depth);
        LOG_ERROR("  This indicates infinite recursion - ABORTING to prevent crash");
        LOG_ERROR("  Query: %.200s", zSql ? zSql : "NULL");
        prepare_v2_depth--;
        if (ppStmt) *ppStmt = NULL;
        if (pzTail) *pzTail = NULL;
        return SQLITE_ERROR;
    }

    // CRITICAL FIX: Stack overflow protection
    // Get thread stack bounds to detect how much stack we have left
    pthread_t self = pthread_self();
    void *stack_addr = NULL;
    size_t stack_size = 0;

#ifdef __APPLE__
    // macOS: use non-portable pthread functions
    stack_addr = pthread_get_stackaddr_np(self);
    stack_size = pthread_get_stacksize_np(self);
#else
    // Linux: use pthread_attr_getstack via pthread_getattr_np
    pthread_attr_t attr;
    void *stack_bottom = NULL;
    if (pthread_getattr_np(self, &attr) == 0) {
        pthread_attr_getstack(&attr, &stack_bottom, &stack_size);
        // On Linux, stack_addr is the BOTTOM of the stack
        // Adjust to get the TOP (where stack starts)
        stack_addr = (char*)stack_bottom + stack_size;
        pthread_attr_destroy(&attr);
    }
#endif

    // Calculate stack base and current position
    char *stack_base = (char*)stack_addr;
    volatile char local_var;  // volatile prevents compiler optimization
    char *current_stack = (char*)&local_var;

    // Calculate how much stack we've used
    // Stack grows downward on both macOS/ARM64 and Linux
    ptrdiff_t stack_used = stack_base - current_stack;
    if (stack_used < 0) stack_used = -stack_used;

#ifndef __APPLE__
    // Linux sanity check: verify current_stack is within stack bounds
    if (stack_bottom && stack_addr) {
        if (current_stack < (char*)stack_bottom || current_stack > (char*)stack_addr) {
            LOG_ERROR("STACK CALCULATION ERROR: current=%p not in [%p, %p]",
                     (void*)current_stack, stack_bottom, stack_addr);
            // Fall back to safe defaults - don't trigger protection on bad calculation
            stack_size = 8 * 1024 * 1024;  // Assume 8MB
            stack_used = 0;
        }
    }
#endif

    // Calculate how much stack is left
    ptrdiff_t stack_remaining = (ptrdiff_t)stack_size - stack_used;

    // DEBUG: Log stack info periodically to verify protection is active
    static __thread int stack_log_counter = 0;
    if (++stack_log_counter == 1 || stack_log_counter % 1000 == 0) {
        LOG_INFO("STACK_CHECK: size=%ldKB used=%ldKB remaining=%ldKB (threshold=64KB)",
                 (long)stack_size/1024, (long)stack_used/1024, (long)stack_remaining/1024);
    }

    // WORKER THREAD DELEGATION:
    // Delegate to 8MB stack worker thread when main thread stack is low
    if (!from_worker && stack_remaining < WORKER_DELEGATION_THRESHOLD && worker_running) {
        LOG_DEBUG("WORKER DELEGATION: stack_remaining=%ld bytes < %d, delegating to 8MB worker",
                 (long)stack_remaining, WORKER_DELEGATION_THRESHOLD);
        prepare_v2_depth--;  // Worker will increment again
        return delegate_prepare_to_worker(db, zSql, nByte, ppStmt, pzTail);
    }

    // CRITICAL FIX: OnDeck queries with low stack cause Plex to crash AFTER query completes
    // When stack < 100KB, Plex's Metal initialization (for thumbnails) crashes in dyld
    // OnDeck queries are identified by their SQL pattern, not URL parameters
    // Must check BEFORE the 8KB threshold since crash happens with ~50KB remaining
    int is_ondeck_query = zSql && (
        (strcasestr(zSql, "metadata_item_settings") && strcasestr(zSql, "metadata_items")) ||
        (strcasestr(zSql, "metadata_item_views") && strcasestr(zSql, "grandparents")) ||
        strcasestr(zSql, "grandparentsSettings")
    );

    // For OnDeck queries with low stack, use PostgreSQL path with minimal stack
    // This avoids the "return empty" workaround that breaks functionality
    if (is_ondeck_query && stack_remaining < 100000) {
        LOG_INFO("STACK LOW OnDeck: %ld bytes remaining - using PG fast path",
                 (long)stack_remaining);
        
        pg_connection_t *pg_conn = pg_find_connection(db);
        // v0.9.4.6: Only use PG path for library.db
        if (pg_conn && pg_conn->is_pg_active && pg_conn->conn &&
            is_library_db_path(pg_conn->db_path)) {
            // Prepare minimal SQLite statement, route to PostgreSQL
            int rc;
            if (shim_sqlite3_prepare_v2) {
                rc = shim_sqlite3_prepare_v2(db, "SELECT 1", -1, ppStmt, pzTail);
            } else {
                rc = SQLITE_ERROR;
                if (ppStmt) *ppStmt = NULL;
            }
            
            if (rc == SQLITE_OK && *ppStmt) {
                pg_stmt_t *pg_stmt = pg_stmt_create(pg_conn, zSql, *ppStmt);
                if (pg_stmt) {
                    pg_stmt->is_pg = 2;  // read operation
                    
                    // Translate query for PostgreSQL
                    sql_translation_t trans = sql_translate(zSql);
                    if (trans.success && trans.sql) {
                        char *aliased = maybe_alias_collection_sync_aggregates(zSql, trans.sql);
                        pg_stmt->pg_sql = strdup(aliased ? aliased : trans.sql);
                        if (aliased) rust_free_cstring(aliased);
                        pg_stmt->param_count = trans.param_count;
                        trace_prepare_pgsql_if_enabled(zSql, pg_stmt->pg_sql);
                        LOG_INFO("STACK LOW OnDeck: routed to PG: %.100s", trans.sql);
                    }
                    sql_translation_free(&trans);
                }
            }
            prepare_v2_depth--;
            return rc;
        }
        
        // No PG connection - fall back to empty result
        LOG_ERROR("STACK CRITICAL OnDeck: no PG connection, returning empty");
        int rc;
        if (shim_sqlite3_prepare_v2) {
            rc = shim_sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, ppStmt, pzTail);
        } else {
            rc = SQLITE_ERROR;
            if (ppStmt) *ppStmt = NULL;
        }
        prepare_v2_depth--;
        return rc;
    }

    // Hard stack threshold - increased from 8KB to 64KB for safety
    // 64KB gives SQLite enough room for simple queries without crashing
    int stack_threshold = from_worker ? 32000 : 64000;

    if (stack_remaining < stack_threshold) {
        // For PostgreSQL-destined read queries, use a minimal SQLite query
        // to get a valid statement handle, then route execution to PostgreSQL
        pg_connection_t *pg_conn_check = pg_find_connection(db);
        // v0.9.4.6: Only use PG path for library.db
        int is_pg_read = pg_conn_check && pg_conn_check->is_pg_active &&
                         pg_conn_check->conn && zSql && is_read_operation(zSql) &&
                         is_library_db_path(pg_conn_check->db_path);

        if (is_pg_read) {
            LOG_INFO("STACK LOW (%ld bytes) but using PG path for: %.100s",
                     (long)stack_remaining, zSql);

            // Prepare a minimal "SELECT 1" to get a valid statement handle
            // The actual query will be executed by PostgreSQL in step()
            int rc;
            if (shim_sqlite3_prepare_v2) {
                rc = shim_sqlite3_prepare_v2(db, "SELECT 1", -1, ppStmt, pzTail);
            } else {
                rc = SQLITE_ERROR;
            }

            if (rc == SQLITE_OK && *ppStmt) {
                // Create PG statement with the REAL query
                pg_stmt_t *pg_stmt = pg_stmt_create(pg_conn_check, zSql, *ppStmt);
                if (pg_stmt) {
                    pg_stmt->is_pg = 2;  // read operation

                    // Translate the query
                    sql_translation_t trans = sql_translate(zSql);
                    if (trans.success && trans.sql) {
                        char *aliased = maybe_alias_collection_sync_aggregates(zSql, trans.sql);
                        pg_stmt->pg_sql = strdup(aliased ? aliased : trans.sql);
                        if (aliased) rust_free_cstring(aliased);
                        pg_stmt->param_count = trans.param_count;
                        trace_prepare_pgsql_if_enabled(zSql, pg_stmt->pg_sql);

                        // Store parameter names
                        if (trans.param_names && trans.param_count > 0) {
                            pg_stmt->param_names = malloc(trans.param_count * sizeof(char*));
                            if (pg_stmt->param_names) {
                                for (int i = 0; i < trans.param_count; i++) {
                                    pg_stmt->param_names[i] = trans.param_names[i] ?
                                                              strdup(trans.param_names[i]) : NULL;
                                }
                            }
                        }

                        // Set up prepared statement caching
                        if (pg_stmt->pg_sql) {
                            pg_stmt->sql_hash = pg_hash_sql(pg_stmt->pg_sql);
                            snprintf(pg_stmt->stmt_name, sizeof(pg_stmt->stmt_name),
                                     "ps_%llx", (unsigned long long)pg_stmt->sql_hash);
                            pg_stmt->use_prepared = 1;
                        }
                    }
                    sql_translation_free(&trans);
                    pg_register_stmt(*ppStmt, pg_stmt);
                }
            }

            prepare_v2_depth--;
            return rc;
        }

        // For non-PG queries or writes, reject with error
        LOG_ERROR("STACK PROTECTION TRIGGERED: stack_used=%ld/%ld bytes, remaining=%ld bytes",
                 (long)stack_used, (long)stack_size, (long)stack_remaining);
        LOG_ERROR("  Query rejected (not a PG read): %.200s", zSql ? zSql : "NULL");

        pg_connection_t *pg_conn = pg_find_connection(db);
        if (pg_conn) {
            pg_conn->last_error_code = SQLITE_NOMEM;
            snprintf(pg_conn->last_error, sizeof(pg_conn->last_error),
                     "Stack protection: insufficient stack space (remaining=%ld).",
                     (long)stack_remaining);
        }

        prepare_v2_depth--;
        if (ppStmt) *ppStmt = NULL;
        if (pzTail) *pzTail = NULL;
        return SQLITE_NOMEM;
    }

    // Skip complex processing only if stack is really tight (not on worker)
    int skip_complex_processing = 0;
    if (!from_worker && stack_remaining < 64000) {
        skip_complex_processing = 1;
        LOG_INFO("STACK CAUTION: stack_used=%ld/%ld bytes, remaining=%ld - skipping complex processing",
                 (long)stack_used, (long)stack_size, (long)stack_remaining);
    }

    // CRITICAL FIX: NULL check to prevent crash in strcasestr
    if (!zSql) {
        LOG_ERROR("prepare_v2 called with NULL SQL");
        int rc;
        if (shim_sqlite3_prepare_v2) {
            rc = shim_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
        } else {
            rc = SQLITE_ERROR;
            if (ppStmt) *ppStmt = NULL;
        }
        prepare_v2_depth--;  // Decrement before return
        return rc;
    }

    // DEBUG: Log queries with backticks (the failing OnDeck query pattern)
    if (strchr(zSql, '`')) {
        LOG_DEBUG("BACKTICK_QUERY: skip_complex=%d len=%d sql=%.200s",
                 skip_complex_processing, (int)strlen(zSql), zSql);
    }

    // Debug: log INSERT INTO metadata_items
    if (!skip_complex_processing && strncasecmp(zSql, "INSERT", 6) == 0 && strcasestr(zSql, "metadata_items")) {
        LOG_INFO("PREPARE_V2 INSERT metadata_items: %.300s", zSql);
        if (strcasestr(zSql, "icu_root")) {
            LOG_INFO("PREPARE_V2 has icu_root - will clean!");
        }
    }


    pg_connection_t *pg_conn = skip_complex_processing ? NULL : pg_find_connection(db);
    int is_write = is_write_operation(zSql);
    int is_read = is_read_operation(zSql);

    if (is_txn_control_sql(zSql)) {
        unsigned long total = __sync_add_and_fetch(&g_txn_route_total, 1);
        int skip_now = should_skip_sql(zSql);
        if (skip_now) __sync_add_and_fetch(&g_txn_route_skipped, 1);
        if (pg_conn && pg_conn->is_pg_active && is_library_db_path(pg_conn->db_path) &&
            (is_read || is_write) && !skip_now) {
            __sync_add_and_fetch(&g_txn_route_pg, 1);
        }
        LOG_INFO("TXN_ROUTE prepare: skip=%d is_write=%d is_read=%d sql=%.220s",
                 skip_now, is_write, is_read, zSql);
        if (total == 1 || total % 50 == 0) {
            unsigned long skipped = g_txn_route_skipped;
            unsigned long routed_pg = g_txn_route_pg;
            LOG_INFO("TXN_ROUTE stats: total=%lu skipped=%lu pg_routed=%lu",
                     total, skipped, routed_pg);
        }
    }

    if (zSql && strcasestr(zSql, "plugins")) {
        LOG_INFO("SKIP_DEBUG plugins query skip=%d sql=%.220s", should_skip_sql(zSql), zSql);
    }

    // =========================================================================
    // Shadow SQLite Prepare: DUMMY for PG-routed, REAL for non-PG
    // =========================================================================
    // For queries routed to PostgreSQL, prepare a dummy SQL on the shadow
    // SQLite that absorbs all bind calls. Column metadata comes from PG
    // via PQdescribePrepared (no need for real SQL on shadow).
    // For non-PG queries (DDL, PRAGMA, non-library), use real SQL.

    char *cleaned_sql = NULL;
    const char *sql_for_sqlite = zSql;
    int use_dummy_shadow = 0;
    int rc;

    // Determine if this query will be routed to PostgreSQL
    // If so, use dummy shadow prepare — no need for real SQL on shadow.
    // Both library.db AND blobs.db go through PG with dummy shadow.
    // blobs.db queries get schema_migrations -> blobs_schema_migrations rewrite
    // in the pg_stmt creation path below.
    if (pg_conn && pg_conn->is_pg_active && is_library_db_path(pg_conn->db_path) &&
        (is_read || is_write) && !should_skip_sql(zSql)) {
        use_dummy_shadow = 1;
    }

    // Pre-translate once; reused for both dummy shadow and pg_stmt creation
    sql_translation_t pre_trans = {0};
    int have_pre_trans = 0;

    if (use_dummy_shadow) {
        // PG-routed query: prepare dummy SQL on shadow SQLite
        // Translate once — result is reused later for pg_stmt creation
        pre_trans = sql_translate(zSql);
        have_pre_trans = 1;
        if (strcasestr(zSql, "json_each(")) {
            LOG_INFO("JSON_EACH_TRANSLATE: orig=%.220s", zSql);
            LOG_INFO("JSON_EACH_TRANSLATE: rc=%d err=%s out=%.220s",
                     pre_trans.success,
                     pre_trans.error[0] ? pre_trans.error : "(null)",
                     pre_trans.sql ? pre_trans.sql : "(null)");
        }
        if (strcasestr(zSql, "metadata_item_settings") &&
            strcasestr(zSql, "metadata_items")) {
            int q_count = 0;
            for (const char *p = zSql; *p; p++) if (*p == '?') q_count++;
            int out_q_count = 0;
            if (pre_trans.sql) {
                for (const char *p = pre_trans.sql; *p; p++) if (*p == '?') out_q_count++;
            }
            LOG_INFO("MIS_TRANSLATE: orig=%s", zSql);
            LOG_INFO("MIS_TRANSLATE: rc=%d params=%d q_orig=%d q_out=%d out=%s",
                     pre_trans.success,
                     pre_trans.param_count,
                     q_count,
                     out_q_count,
                     pre_trans.sql ? pre_trans.sql : "(null)");
        }

        {
            int orig_q = 0;
            for (const char *p = zSql; p && *p; p++) if (*p == '?') orig_q++;
            if (orig_q > pre_trans.param_count) {
                const char *qpos = strchr(zSql, '?');
                if (qpos) {
                    int start = (int)(qpos - zSql) - 60;
                    if (start < 0) start = 0;
                    LOG_ERROR("PLACEHOLDER_MISMATCH: orig_q=%d translated_params=%d around='%.160s'",
                              orig_q, pre_trans.param_count, zSql + start);
                } else {
                    LOG_ERROR("PLACEHOLDER_MISMATCH: orig_q=%d translated_params=%d (no snippet)",
                              orig_q, pre_trans.param_count);
                }
            }
        }
        int param_count = pre_trans.param_count;

        // Build dummy SQL that absorbs all bind calls.
        // Use named params (:name) when available so that
        // sqlite3_bind_parameter_index(":name") returns the correct index.
        // Fall back to positional ? when names are not available.
        char dummy_sql[4096];
        if (param_count == 0) {
            snprintf(dummy_sql, sizeof(dummy_sql), "SELECT 1 WHERE 0");
        } else {
            int has_names = (pre_trans.param_names != NULL);
            int off = snprintf(dummy_sql, sizeof(dummy_sql), "SELECT 1 WHERE ");
            for (int i = 0; i < param_count; i++) {
                if (i > 0) off += snprintf(dummy_sql + off, sizeof(dummy_sql) - off, " AND ");
                if (has_names && pre_trans.param_names[i]) {
                    // Use :<name> so SQLite registers the named parameter
                    off += snprintf(dummy_sql + off, sizeof(dummy_sql) - off,
                                    ":%s IS NOT NULL", pre_trans.param_names[i]);
                } else {
                    off += snprintf(dummy_sql + off, sizeof(dummy_sql) - off, "? IS NOT NULL");
                }
                if (off >= (int)sizeof(dummy_sql) - 40) break;
            }
        }

        if (shim_sqlite3_prepare_v2) {
            rc = shim_sqlite3_prepare_v2(db, dummy_sql, -1, ppStmt, pzTail);
        } else {
            LOG_ERROR("CRITICAL: shim_sqlite3_prepare_v2 not initialized!");
            rc = SQLITE_ERROR;
            if (ppStmt) *ppStmt = NULL;
        }

        if (rc != SQLITE_OK || !*ppStmt) {
            LOG_ERROR("PREPARE: Dummy shadow prepare failed (rc=%d, params=%d): %.100s dummy=%.200s",
                     rc, param_count, zSql, dummy_sql);
            sql_translation_free(&pre_trans);
            prepare_v2_depth--;
            return rc;
        }

        LOG_DEBUG("PREPARE: Dummy shadow OK (%d params) for PG query: %.100s", param_count, zSql);
    } else {
        // Non-PG query: prepare real SQL on shadow SQLite
        // Clean SQL for SQLite (remove icu_root and FTS references)

        // ALWAYS simplify FTS queries for SQLite, even without PG connection
        // because SQLite shadow DB doesn't have FTS virtual tables
        if (!skip_complex_processing && strcasestr(zSql, "fts4_")) {
            cleaned_sql = simplify_fts_for_sqlite(zSql);
            if (cleaned_sql) {
                sql_for_sqlite = cleaned_sql;
                LOG_INFO("FTS query ORIGINAL: %.500s", zSql);
                LOG_INFO("FTS query SIMPLIFIED: %.500s", cleaned_sql);
            }
        }

        // ALWAYS remove "collate icu_root" since SQLite shadow DB doesn't support it
        if (!skip_complex_processing && strcasestr(sql_for_sqlite, "collate icu_root")) {
            char *temp = strip_collate_icu_root_for_sqlite(sql_for_sqlite);
            if (temp) {
                if (cleaned_sql) free(cleaned_sql);
                cleaned_sql = temp;
                sql_for_sqlite = cleaned_sql;
            }
        }

        // Block remaining FTS queries from shadow SQLite
        if (strcasestr(sql_for_sqlite, "fts4_") || strcasestr(sql_for_sqlite, " match ")) {
            LOG_INFO("FTS query blocked from SQLite (tokenizer not available): %.100s", sql_for_sqlite);
            if (shim_sqlite3_prepare_v2) {
                int rc = shim_sqlite3_prepare_v2(db, "SELECT 1 WHERE 0", -1, ppStmt, pzTail);
                if (cleaned_sql) free(cleaned_sql);
                prepare_v2_depth--;
                return rc;
            }
        }

        // Add IF NOT EXISTS for CREATE TABLE/INDEX sent to shadow SQLite
        if (!skip_complex_processing && sql_for_sqlite) {
            char *ine_sql = add_if_not_exists_for_sqlite_ddl(sql_for_sqlite);
            if (ine_sql) {
                if (cleaned_sql) free(cleaned_sql);
                cleaned_sql = ine_sql;
                sql_for_sqlite = cleaned_sql;
                LOG_INFO("Added IF NOT EXISTS for SQLite DDL: %.200s", sql_for_sqlite);
            }
        }

        // Prepare real SQL on shadow SQLite
        if (shim_sqlite3_prepare_v2) {
            rc = shim_sqlite3_prepare_v2(db, sql_for_sqlite, cleaned_sql ? -1 : nByte, ppStmt, pzTail);
        } else {
            LOG_ERROR("CRITICAL: shim_sqlite3_prepare_v2 not initialized!");
            rc = SQLITE_ERROR;
            if (ppStmt) *ppStmt = NULL;
        }

        if (rc != SQLITE_OK || !*ppStmt) {
            // Log the failure for debugging — especially important for fresh Docker installs
            // where blobs.db schema_migrations queries can fail unexpectedly
            const char *sqlite_err = orig_sqlite3_errmsg ? orig_sqlite3_errmsg(db) : "unknown";
            int sqlite_errcode = orig_sqlite3_errcode ? orig_sqlite3_errcode(db) : -1;
            LOG_ERROR("PREPARE_REAL_SQLITE FAILED: rc=%d errcode=%d errmsg='%s' sql=%.200s",
                     rc, sqlite_errcode, sqlite_err ? sqlite_err : "NULL",
                     sql_for_sqlite ? sql_for_sqlite : "NULL");
            if (cleaned_sql) free(cleaned_sql);
            prepare_v2_depth--;
            return rc;
        }
    }

    // CRITICAL FIX: Clear our tracked error state on success
    pg_connection_t *pg_conn_for_clear = pg_find_connection(db);
    if (pg_conn_for_clear) {
        pg_conn_for_clear->last_error_code = SQLITE_OK;
        pg_conn_for_clear->last_error[0] = '\0';
    }

    // v0.9.4.6: Only create pg_stmt for library.db
    // Non-library databases (blobs.db, etc.) should use SQLite directly.
    // Without this check, blobs.db queries get pg_stmt with is_pg != 0,
    // but no PostgreSQL result, causing column functions to fail.
    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active && (is_write || is_read) &&
        is_library_db_path(pg_conn->db_path)) {
        pg_stmt_t *pg_stmt = pg_stmt_create(pg_conn, zSql, *ppStmt);
        if (pg_stmt) {
            if (should_skip_sql(zSql)) {
                pg_stmt->is_pg = 3;  // skip
            } else {
                pg_stmt->is_pg = is_write ? 1 : 2;

                // Reuse pre_trans from dummy path, or translate now for non-dummy path
                sql_translation_t trans;
                if (have_pre_trans) {
                    trans = pre_trans;
                    // Clear so we don't double-free later
                    memset(&pre_trans, 0, sizeof(pre_trans));
                    have_pre_trans = 0;
                } else {
                    trans = sql_translate(zSql);
                }
                if (!trans.success) {
                       LOG_ERROR("Translation failed for SQL: %s. Error: %s", zSql, trans.error);
                }

                pg_stmt->param_count = trans.param_count;

                // Store parameter names for mapping named parameters
                if (trans.param_names && trans.param_count > 0) {
                    pg_stmt->param_names = malloc(trans.param_count * sizeof(char*));
                    if (pg_stmt->param_names) {
                        for (int i = 0; i < trans.param_count; i++) {
                            pg_stmt->param_names[i] = trans.param_names[i] ? strdup(trans.param_names[i]) : NULL;
                        }
                    }

                }

                if (trans.success && trans.sql) {
                    // Rewrite blobs.db schema_migrations → blobs_schema_migrations
                    char *blobs_rewrite = rewrite_blobs_schema_migrations(trans.sql, pg_conn->db_path);
                    const char *effective_sql = blobs_rewrite ? blobs_rewrite : trans.sql;

                    char *aliased = maybe_alias_collection_sync_aggregates(zSql, effective_sql);
                    pg_stmt->pg_sql = strdup(aliased ? aliased : effective_sql);
                    if (aliased) rust_free_cstring(aliased);
                    if (blobs_rewrite) free(blobs_rewrite);
                    trace_prepare_pgsql_if_enabled(zSql, pg_stmt->pg_sql);
                     
                    // PERFORMANCE FIX: Cache count query detection at prepare time (not per-row)
                    pg_stmt->is_count_query = (pg_stmt->pg_sql && 
                                                strstr(pg_stmt->pg_sql, "parents.parent_id,count(*)")) ? 1 : 0;

                    // CRITICAL FIX: Add ON CONFLICT DO NOTHING for schema_migrations INSERTs
                    if (is_write && strncasecmp(zSql, "INSERT", 6) == 0 &&
                        pg_stmt->pg_sql && strcasestr(pg_stmt->pg_sql, "schema_migrations") &&
                        !strcasestr(pg_stmt->pg_sql, "ON CONFLICT")) {
                        size_t len = strlen(pg_stmt->pg_sql);
                        char *with_conflict = malloc(len + 40);
                        if (with_conflict) {
                            snprintf(with_conflict, len + 40, "%s ON CONFLICT DO NOTHING", pg_stmt->pg_sql);
                            LOG_INFO("SCHEMA_MIGRATIONS: Added ON CONFLICT DO NOTHING: %.200s", with_conflict);
                            free(pg_stmt->pg_sql);
                            pg_stmt->pg_sql = with_conflict;
                        }
                    }

                    // Add RETURNING id to INSERT statements for proper ID retrieval
                    if (is_write && strncasecmp(zSql, "INSERT", 6) == 0 &&
                        pg_stmt->pg_sql && !strstr(pg_stmt->pg_sql, "RETURNING") &&
                        !strcasestr(pg_stmt->pg_sql, "schema_migrations")) {
                        size_t len = strlen(pg_stmt->pg_sql);
                        char *with_returning = malloc(len + 20);
                        if (with_returning) {
                            snprintf(with_returning, len + 20, "%s RETURNING id", pg_stmt->pg_sql);
                            if (strstr(pg_stmt->pg_sql, "play_queue_generators")) {
                                LOG_INFO("PREPARE play_queue_generators INSERT with RETURNING: %s", with_returning);
                            }
                            free(pg_stmt->pg_sql);
                            pg_stmt->pg_sql = with_returning;
                        }
                    }

                    // Calculate hash and statement name for prepared statement support
                    if (pg_stmt->pg_sql) {
                        pg_stmt->sql_hash = pg_hash_sql(pg_stmt->pg_sql);
                        snprintf(pg_stmt->stmt_name, sizeof(pg_stmt->stmt_name),
                                 "ps_%llx", (unsigned long long)pg_stmt->sql_hash);
                        pg_stmt->use_prepared = 1;
                    }
                }
                sql_translation_free(&trans);
            }

            pg_register_stmt(*ppStmt, pg_stmt);
        }
    }

    if (have_pre_trans) sql_translation_free(&pre_trans);
    if (cleaned_sql) free(cleaned_sql);
    prepare_v2_depth--;  // Decrement before return
    return rc;
}

// Plex SyncCollections uses a query that selects aggregate expressions without
// explicit aliases: count(*), min(year), max(year). SQLite exposes the full
// expression text via sqlite3_column_name() (e.g. "min(year)"). PostgreSQL
// reports only the function name ("min"/"max"/"count") unless an alias is
// provided, which can break Plex's row mapping and lead to std::bad_cast.
//
// This helper adds explicit aliases matching SQLite's column names.
static char *maybe_alias_collection_sync_aggregates(const char *sqlite_sql, const char *pg_sql) {
    return rust_maybe_alias_collection_sync_aggregates(sqlite_sql, pg_sql);
}

// ============================================================================
// Public Prepare Functions
// ============================================================================

// Public wrapper - delegates to worker thread if stack is low
int my_sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                          sqlite3_stmt **ppStmt, const char **pzTail) {
    // CRITICAL: Ensure real SQLite is loaded (may be called before constructor!)
    ensure_real_sqlite_loaded();

    // CRITICAL FIX: Prevent infinite recursion when our internal code calls sqlite3_prepare_v2
    // With DYLD_INTERPOSE, ALL calls to sqlite3_prepare_v2 come through here, including
    // our own internal calls on lines 711 and 770. Use thread-local flag to detect this.
    if (in_interpose_call) {
        // We're already inside our shim - this is a recursive call from our own code.
        // Call the REAL sqlite3_prepare_v2 directly via our resolved function pointer.
        // This bypasses DYLD_INTERPOSE and prevents infinite recursion.
        if (shim_sqlite3_prepare_v2) {
            return shim_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
        } else {
            // Fallback if real function pointer wasn't resolved (should never happen)
            LOG_ERROR("CRITICAL: shim_sqlite3_prepare_v2 is NULL during recursive call!");
            return SQLITE_ERROR;
        }
    }

    in_interpose_call = 1;
    int result = my_sqlite3_prepare_v2_internal(db, zSql, nByte, ppStmt, pzTail, 0);
    in_interpose_call = 0;
    return result;
}

int my_sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail) {
    // Route through my_sqlite3_prepare_v2 to get icu_root cleanup and PG handling
    return my_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}

int my_sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte,
                            sqlite3_stmt **ppStmt, const void **pzTail) {
    // Convert UTF-16 to UTF-8 for icu_root cleanup
    // This is rarely used but we need to handle it for completeness
    if (zSql) {
        // Get UTF-16 length
        int utf16_len = 0;
        if (nByte < 0) {
            const uint16_t *p = (const uint16_t *)zSql;
            while (*p) { p++; utf16_len++; }
            utf16_len *= 2;
        } else {
            utf16_len = nByte;
        }

        // Convert to UTF-8 using a simple approach
        char *utf8_sql = malloc(utf16_len * 2 + 1);
        if (utf8_sql) {
            const uint16_t *src = (const uint16_t *)zSql;
            char *dst = utf8_sql;
            int i;
            for (i = 0; i < utf16_len / 2 && src[i]; i++) {
                if (src[i] < 0x80) {
                    *dst++ = (char)src[i];
                } else if (src[i] < 0x800) {
                    *dst++ = 0xC0 | (src[i] >> 6);
                    *dst++ = 0x80 | (src[i] & 0x3F);
                } else {
                    *dst++ = 0xE0 | (src[i] >> 12);
                    *dst++ = 0x80 | ((src[i] >> 6) & 0x3F);
                    *dst++ = 0x80 | (src[i] & 0x3F);
                }
            }
            *dst = '\0';

            // Check for icu_root and route through UTF-8 handler if found
            if (strcasestr(utf8_sql, "collate icu_root")) {
                LOG_INFO("UTF-16 query with icu_root, routing to UTF-8 handler: %.200s", utf8_sql);
                const char *tail8 = NULL;
                int rc = my_sqlite3_prepare_v2(db, utf8_sql, -1, ppStmt, &tail8);
                free(utf8_sql);
                if (pzTail) *pzTail = NULL;  // Tail not accurate after conversion
                return rc;
            }
            free(utf8_sql);
        }
    }

    return sqlite3_prepare16_v2(db, zSql, nByte, ppStmt, pzTail);
}

int my_sqlite3_prepare_v3(sqlite3 *db, const char *zSql, int nByte,
                          unsigned int prepFlags, sqlite3_stmt **ppStmt,
                          const char **pzTail) {
    // Log that prepare_v3 is being used
    if (zSql && strcasestr(zSql, "metadata_items")) {
        LOG_INFO("PREPARE_V3 metadata_items query: %.200s", zSql);
    }
    // Route through my_sqlite3_prepare_v2 to get icu_root cleanup and PG handling
    // We ignore prepFlags for now as they're SQLite-specific optimizations
    (void)prepFlags;
    return my_sqlite3_prepare_v2(db, zSql, nByte, ppStmt, pzTail);
}
