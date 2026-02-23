/*
 * Plex PostgreSQL Interposing Shim - Open/Close Operations
 *
 * Handles sqlite3_open, sqlite3_open_v2, sqlite3_close, sqlite3_close_v2.
 * Also includes helpers for dropping ICU indexes and FTS triggers.
 */

#include "db_interpose.h"

// NOTE: drop_icu_root_indexes() and drop_fts_triggers() were removed in v0.9.32
// as part of shadow elimination. With :memory: shadow, there are no ICU indexes
// or FTS triggers to clean up — the shadow has no schema at all.

// ============================================================================
// Open Functions
// ============================================================================

int my_sqlite3_open(const char *filename, sqlite3 **ppDb) {
    LOG_INFO("OPEN: %s (redirect=%d)", filename ? filename : "(null)", should_redirect(filename));

    // Always open the real file — Plex needs the SQLite schema for PRAGMA,
    // sqlite_master queries, and SOCI session setup. The dummy prepare path
    // ensures all data queries go to PostgreSQL anyway.
    int rc = orig_sqlite3_open ? orig_sqlite3_open(filename, ppDb) : SQLITE_ERROR;

    if (rc == SQLITE_OK && should_redirect(filename)) {
        pg_connection_t *pg_conn = pg_connect(filename, *ppDb);
        if (pg_conn) {
            pg_register_connection(pg_conn);
            LOG_INFO("PostgreSQL connection established for: %s", filename);
        }
    }

    return rc;
}

int my_sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs) {
    LOG_INFO("OPEN_V2: %s flags=0x%x (redirect=%d)",
             filename ? filename : "(null)", flags, should_redirect(filename));

    int rc = orig_sqlite3_open_v2 ? orig_sqlite3_open_v2(filename, ppDb, flags, zVfs) : SQLITE_ERROR;

    if (rc == SQLITE_OK && should_redirect(filename)) {
        pg_connection_t *pg_conn = pg_connect(filename, *ppDb);
        if (pg_conn) {
            pg_register_connection(pg_conn);
            LOG_INFO("PostgreSQL connection established for: %s", filename);
        }
    }

    return rc;
}

// ============================================================================
// Close Functions
// ============================================================================

int my_sqlite3_close(sqlite3 *db) {
    // Get the handle connection (NOT pool connection) for this db
    pg_connection_t *handle_conn = pg_find_handle_connection(db);
    if (handle_conn) {
        LOG_INFO("CLOSE: PostgreSQL connection for %s", handle_conn->db_path);

        // If this is a library.db, release pool connection back to pool (don't free it!)
        if (strstr(handle_conn->db_path, "com.plexapp.plugins.library.db")) {
            pg_close_pool_for_db(db);
        }

        // Unregister and close the handle connection (not the pool connection)
        pg_unregister_connection(handle_conn);
        pg_close(handle_conn);
    }
    return orig_sqlite3_close ? orig_sqlite3_close(db) : SQLITE_ERROR;
}

int my_sqlite3_close_v2(sqlite3 *db) {
    // Get the handle connection (NOT pool connection) for this db
    pg_connection_t *handle_conn = pg_find_handle_connection(db);
    if (handle_conn) {
        // If this is a library.db, release pool connection back to pool (don't free it!)
        if (strstr(handle_conn->db_path, "com.plexapp.plugins.library.db")) {
            pg_close_pool_for_db(db);
        }

        // Unregister and close the handle connection (not the pool connection)
        pg_unregister_connection(handle_conn);
        pg_close(handle_conn);
    }
    return orig_sqlite3_close_v2 ? orig_sqlite3_close_v2(db) : SQLITE_ERROR;
}
