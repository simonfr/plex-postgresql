/*
 * Plex PostgreSQL Interposing Shim - Exec Operations
 *
 * Handles sqlite3_exec function interposition.
 * 
 * Performance optimization: SQL Normalization
 * - Extracts numeric literals from SQL and converts to parameters
 * - "SELECT * FROM t WHERE id = 123" → "SELECT * FROM t WHERE id = $1" with param "123"
 * - Enables prepared statement reuse for varying SQL (huge performance win)
 * - PQexecPrepared with cached stmt: ~12µs vs PQexec: ~40µs
 */

#include "db_interpose.h"
#include <ctype.h>
#include "shim_alloc.h"

// ============================================================================
// SQL Normalization - Extract numeric literals as parameters
// ============================================================================

#define MAX_NORMALIZED_PARAMS 32

typedef struct {
    char *normalized_sql;      // SQL with $1, $2, etc.
    char *param_values[MAX_NORMALIZED_PARAMS];  // Extracted literal values
    int param_count;
} normalized_sql_t;

// Check if we're inside a string literal or identifier
static int is_inside_string(const char *sql, const char *pos) {
    int in_single = 0, in_double = 0;
    for (const char *p = sql; p < pos; p++) {
        if (*p == '\'' && !in_double) in_single = !in_single;
        else if (*p == '"' && !in_single) in_double = !in_double;
    }
    return in_single || in_double;
}

// Normalize SQL by extracting numeric literals as parameters
// Returns NULL if normalization not applicable or fails
static normalized_sql_t* normalize_sql_literals(const char *sql) {
    if (!sql) return NULL;
    
    // Quick check: only normalize SELECT/UPDATE/DELETE with WHERE
    // Skip INSERT (values are usually all literals, less benefit)
    if (strncasecmp(sql, "INSERT", 6) == 0) return NULL;
    if (!strcasestr(sql, "WHERE")) return NULL;
    
    // Allocate result
    normalized_sql_t *result = calloc(1, sizeof(normalized_sql_t));
    if (!result) return NULL;
    
    // Allocate buffer for normalized SQL (same size + extra for $N placeholders)
    size_t sql_len = strlen(sql);
    char *out = malloc(sql_len + MAX_NORMALIZED_PARAMS * 4);  // Extra space for $NN
    if (!out) {
        free(result);
        return NULL;
    }
    
    const char *p = sql;
    char *o = out;
    int param_idx = 0;
    
    while (*p) {
        // Check for numeric literal (not inside string, preceded by operator/space/paren)
        if (param_idx < MAX_NORMALIZED_PARAMS && 
            (isdigit((unsigned char)*p) || (*p == '-' && isdigit((unsigned char)p[1]))) &&
            !is_inside_string(sql, p)) {
            
            // Check what precedes this number
            char prev = (p > sql) ? *(p-1) : ' ';
            if (prev == '=' || prev == '>' || prev == '<' || prev == ' ' || 
                prev == '(' || prev == ',' || prev == '+' || prev == '-' || 
                prev == '*' || prev == '/' || prev == '%') {
                
                // Extract the number
                const char *num_start = p;
                if (*p == '-') p++;
                while (isdigit((unsigned char)*p)) p++;
                // Handle decimals
                if (*p == '.' && isdigit((unsigned char)p[1])) {
                    p++;
                    while (isdigit((unsigned char)*p)) p++;
                }
                
                // Check what follows (should be operator/space/paren/end)
                char next = *p;
                if (next == '\0' || next == ' ' || next == ')' || next == ',' ||
                    next == ';' || next == '>' || next == '<' || next == '=' ||
                    next == '+' || next == '-' || next == '*' || next == '/' ||
                    strcasecmp(p, " AND") == 0 || strcasecmp(p, " OR") == 0 ||
                    strncasecmp(p, " ORDER", 6) == 0 || strncasecmp(p, " LIMIT", 6) == 0 ||
                    strncasecmp(p, " GROUP", 6) == 0) {
                    
                    // Store the literal value
                    size_t num_len = p - num_start;
                    result->param_values[param_idx] = malloc(num_len + 1);
                    if (result->param_values[param_idx]) {
                        memcpy(result->param_values[param_idx], num_start, num_len);
                        result->param_values[param_idx][num_len] = '\0';
                        param_idx++;
                        
                        // Write placeholder
                        o += sprintf(o, "$%d", param_idx);
                        continue;
                    }
                }
                // Not a replaceable number, reset position
                p = num_start;
            }
        }
        
        *o++ = *p++;
    }
    *o = '\0';
    
    // Only return normalized result if we extracted at least one parameter
    if (param_idx == 0) {
        free(out);
        free(result);
        return NULL;
    }
    
    result->normalized_sql = out;
    result->param_count = param_idx;
    return result;
}

static void free_normalized_sql(normalized_sql_t *n) {
    if (!n) return;
    free(n->normalized_sql);
    for (int i = 0; i < n->param_count; i++) {
        free(n->param_values[i]);
    }
    free(n);
}

// ============================================================================
// Exec Function
// ============================================================================

int my_sqlite3_exec(sqlite3 *db, const char *sql,
                    int (*callback)(void*, int, char**, char**),
                    void *arg, char **errmsg) {
    // CRITICAL FIX: NULL check to prevent crash in strcasestr
    if (!sql) {
        LOG_ERROR("exec called with NULL SQL");
        return orig_sqlite3_exec ? orig_sqlite3_exec(db, sql, callback, arg, errmsg) : SQLITE_ERROR;
    }

    pg_connection_t *pg_conn = pg_find_connection(db);

    if (pg_conn && pg_conn->conn && pg_conn->is_pg_active) {
        // Rewrite schema_migrations for blobs.db connections
        char *blobs_rewrite = rewrite_blobs_schema_migrations(sql, pg_conn->db_path);
        if (blobs_rewrite) sql = blobs_rewrite;

        if (!should_skip_sql(sql)) {
            // GUARD: Block junk INSERTs into metadata_items with NULL library_section_id AND metadata_type
            if (strcasestr(sql, "INSERT") && strcasestr(sql, "metadata_items") &&
                !strcasestr(sql, "metadata_item_settings") &&
                !strcasestr(sql, "metadata_item_views") &&
                !strcasestr(sql, "metadata_item_accounts") &&
                !strcasestr(sql, "metadata_item_clusters")) {
                const char *col_start = strchr(sql, '(');
                if (col_start) {
                    int lib_idx = -1, type_idx = -1, idx = 0;
                    const char *p = col_start + 1;
                    while (*p && *p != ')') {
                        while (*p == ' ' || *p == '"' || *p == '`') p++;
                        if (strncmp(p, "library_section_id", 18) == 0) lib_idx = idx;
                        if (strncmp(p, "metadata_type", 13) == 0 &&
                            (p[13] == '"' || p[13] == '`' || p[13] == ',' || p[13] == ')' || p[13] == ' '))
                            type_idx = idx;
                        while (*p && *p != ',' && *p != ')') p++;
                        if (*p == ',') { p++; idx++; }
                    }
                    const char *vals = strcasestr(sql, "VALUES");
                    if (vals && lib_idx >= 0 && type_idx >= 0) {
                        const char *vp = strchr(vals, '(');
                        if (vp) {
                            vp++;
                            int vi = 0, lib_null = 0, type_null = 0, in_quote = 0;
                            while (*vp && *vp != ')') {
                                while (*vp == ' ') vp++;
                                if (vi == lib_idx && strncasecmp(vp, "NULL", 4) == 0) lib_null = 1;
                                if (vi == type_idx && strncasecmp(vp, "NULL", 4) == 0) type_null = 1;
                                in_quote = 0;
                                while (*vp && (*vp != ',' || in_quote) && *vp != ')') {
                                    if (*vp == '\'') in_quote = !in_quote;
                                    vp++;
                                }
                                if (*vp == ',') { vp++; vi++; }
                            }
                            if (lib_null && type_null) {
                                LOG_ERROR("GUARD: Blocked exec junk INSERT into metadata_items "
                                          "(library_section_id=NULL, metadata_type=NULL)");
                                return SQLITE_OK;
                            }
                        }
                    }
                }
            }

            sql_translation_t trans = sql_translate(sql);
            if (trans.success && trans.sql) {
                char *exec_sql = trans.sql;
                char *insert_sql = NULL;

                // Add RETURNING id for INSERT statements
                if (strncasecmp(sql, "INSERT", 6) == 0 && !strstr(trans.sql, "RETURNING")) {
                    size_t len = strlen(trans.sql);
                    insert_sql = malloc(len + 20);
                    if (insert_sql) {
                        snprintf(insert_sql, len + 20, "%s RETURNING id", trans.sql);
                        exec_sql = insert_sql;
                        if (strstr(sql, "play_queue_generators")) {
                            LOG_INFO("EXEC play_queue_generators INSERT with RETURNING: %s", exec_sql);
                        }
                    }
                }

                // CRITICAL FIX: Lock connection mutex to prevent concurrent libpq access
                pthread_mutex_lock(&pg_conn->mutex);
                
                PGresult *res = NULL;
                
                // PERFORMANCE OPTIMIZATION: SQL Normalization
                // Try to extract numeric literals as parameters for prepared statement reuse
                // "WHERE id = 123" → "WHERE id = $1" with param "123"
                normalized_sql_t *normalized = normalize_sql_literals(exec_sql);
                
                if (normalized) {
                    // Normalization succeeded - use prepared statement with extracted params
                    uint64_t norm_hash = pg_hash_sql(normalized->normalized_sql);
                    const char *cached_stmt_name = NULL;
                    char stmt_name[32];
                    
                    if (pg_stmt_cache_lookup(pg_conn, norm_hash, &cached_stmt_name)) {
                        // Cache HIT - execute with extracted parameters
                        const char *param_ptrs[MAX_NORMALIZED_PARAMS];
                        for (int i = 0; i < normalized->param_count; i++) {
                            param_ptrs[i] = normalized->param_values[i];
                        }
                        res = PQexecPrepared(pg_conn->conn, cached_stmt_name, 
                                            normalized->param_count, param_ptrs, NULL, NULL, 0);
                    } else {
                        // Cache MISS - prepare normalized SQL, then execute
                        snprintf(stmt_name, sizeof(stmt_name), "nx_%llx", (unsigned long long)norm_hash);
                        PGresult *prep_res = PQprepare(pg_conn->conn, stmt_name, 
                                                       normalized->normalized_sql, 0, NULL);
                        if (PQresultStatus(prep_res) == PGRES_COMMAND_OK) {
                            pg_stmt_cache_add(pg_conn, norm_hash, stmt_name, normalized->param_count);
                            PQclear(prep_res);
                            
                            const char *param_ptrs[MAX_NORMALIZED_PARAMS];
                            for (int i = 0; i < normalized->param_count; i++) {
                                param_ptrs[i] = normalized->param_values[i];
                            }
                            res = PQexecPrepared(pg_conn->conn, stmt_name,
                                                normalized->param_count, param_ptrs, NULL, NULL, 0);
                        } else {
                            // Prepare failed - fall back to direct exec
                            PQclear(prep_res);
                            res = PQexec(pg_conn->conn, exec_sql);
                        }
                    }
                    free_normalized_sql(normalized);
                } else {
                    // Normalization not applicable - try regular prepared stmt cache or direct exec
                    uint64_t sql_hash = pg_hash_sql(exec_sql);
                    const char *cached_stmt_name = NULL;
                    
                    if (pg_stmt_cache_lookup(pg_conn, sql_hash, &cached_stmt_name)) {
                        // Cache HIT for exact SQL match
                        res = PQexecPrepared(pg_conn->conn, cached_stmt_name, 0, NULL, NULL, NULL, 0);
                    } else {
                        // Cache MISS - use PQexec directly (1 round-trip)
                        res = PQexec(pg_conn->conn, exec_sql);
                    }
                }
                
                ExecStatusType status = PQresultStatus(res);

                if (status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK) {
                    pg_conn->last_changes = atoi(PQcmdTuples(res) ?: "1");

                    // Extract ID from RETURNING clause for INSERT
                    if (strncasecmp(sql, "INSERT", 6) == 0 && status == PGRES_TUPLES_OK && PQntuples(res) > 0) {
                        const char *id_str = PQgetvalue(res, 0, 0);
                        if (id_str && *id_str) {
                            if (strstr(sql, "play_queue_generators")) {
                                LOG_INFO("EXEC play_queue_generators: RETURNING id = %s", id_str);
                            }
                            sqlite3_int64 meta_id = extract_metadata_id_from_generator_sql(sql);
                            if (meta_id > 0) pg_set_global_metadata_id(meta_id);
                        }
                    }
                } else {
                    const char *err = (pg_conn && pg_conn->conn) ? PQerrorMessage(pg_conn->conn) : "NULL connection";
                    LOG_ERROR("PostgreSQL exec error: %s", err);
                    // CRITICAL: Check if connection is corrupted and needs reset
                    // Note: pg_conn may be a pool connection for library.db
                    pg_pool_check_connection_health(pg_conn);
                }

                if (insert_sql) free(insert_sql);
                PQclear(res);
                pthread_mutex_unlock(&pg_conn->mutex);
            }
            sql_translation_free(&trans);
        }
        if (blobs_rewrite) free(blobs_rewrite);
        return SQLITE_OK;
    }

    // For non-PG databases, strip collate icu_root since SQLite doesn't support it
    char *cleaned_sql = NULL;
    const char *exec_sql = sql;
    if (strcasestr(sql, "collate icu_root")) {
        cleaned_sql = strdup(sql);
        if (cleaned_sql) {
            char *pos;
            while ((pos = strcasestr(cleaned_sql, " collate icu_root")) != NULL) {
                memmove(pos, pos + 17, strlen(pos + 17) + 1);
            }
            while ((pos = strcasestr(cleaned_sql, "collate icu_root")) != NULL) {
                memmove(pos, pos + 16, strlen(pos + 16) + 1);
            }
            exec_sql = cleaned_sql;
        }
    }

    int rc = orig_sqlite3_exec ? orig_sqlite3_exec(db, exec_sql, callback, arg, errmsg) : SQLITE_ERROR;
    if (cleaned_sql) free(cleaned_sql);
    return rc;
}
