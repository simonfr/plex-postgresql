/*
 * SQL Translator - Main Orchestrator
 * SQLite to PostgreSQL SQL translation
 *
 * This file coordinates all translation modules:
 * - sql_tr_helpers.c     - String utilities
 * - sql_tr_placeholders.c - Placeholder translation
 * - sql_tr_functions.c   - Function translations
 * - sql_tr_query.c       - Query structure fixes
 * - sql_tr_types.c       - Type translations
 * - sql_tr_quotes.c      - Quote translations
 * - sql_tr_keywords.c    - Keyword translations
 */

#include "sql_translator.h"
#include "sql_translator_internal.h"
#include "pg_logging.h"
#include <stdint.h>
#include "shim_alloc.h"

// ============================================================================
// Thread-Local Translation Cache (lock-free, ~500x speedup for cache hits)
// ============================================================================

#define TRANS_CACHE_SIZE 512
#define TRANS_CACHE_MASK (TRANS_CACHE_SIZE - 1)

typedef struct {
    uint64_t hash;           // FNV-1a hash of input SQL (0 = empty)
    char *input_sql;         // Original SQLite SQL (for collision check)
    char *output_sql;        // Translated PostgreSQL SQL
    int param_count;         // Number of parameters
    char **param_names;      // Named parameter names (e.g., ["C1", "C2", ...])
} trans_cache_entry_t;

// Thread-local cache - no locks needed!
static __thread trans_cache_entry_t trans_cache[TRANS_CACHE_SIZE];
static __thread int trans_cache_initialized = 0;

// FNV-1a hash
static uint64_t hash_sql(const char *sql) {
    uint64_t h = 14695981039346656037ULL;
    while (*sql) {
        h ^= (uint64_t)(unsigned char)*sql++;
        h *= 1099511628211ULL;
    }
    return h;
}

// Lookup in thread-local cache
static sql_translation_t* cache_lookup(const char *sql, uint64_t hash) {
    static __thread sql_translation_t cached_result;
    
    int start_idx = (int)(hash & TRANS_CACHE_MASK);
    
    for (int probe = 0; probe < 8; probe++) {
        int idx = (start_idx + probe) & TRANS_CACHE_MASK;
        trans_cache_entry_t *entry = &trans_cache[idx];
        
        if (entry->hash == 0) {
            return NULL;  // Empty slot - not found
        }
        
        if (entry->hash == hash && entry->input_sql && strcmp(entry->input_sql, sql) == 0) {
            // Found! Return cached result
            cached_result.sql = entry->output_sql;  // Note: caller must NOT free this
            cached_result.param_names = entry->param_names;  // Cached (needed for dummy shadow prepare)
            cached_result.param_count = entry->param_count;
            cached_result.success = 1;
            cached_result.error[0] = '\0';
            return &cached_result;
        }
    }
    
    return NULL;
}

// Free cached param_names array in a cache entry
static void cache_free_param_names(trans_cache_entry_t *entry) {
    if (entry->param_names) {
        for (int i = 0; i < entry->param_count; i++) {
            free(entry->param_names[i]);
        }
        free(entry->param_names);
        entry->param_names = NULL;
    }
}

// Deep-copy param_names array into a cache entry
static void cache_store_param_names(trans_cache_entry_t *entry, char **param_names, int param_count) {
    cache_free_param_names(entry);
    entry->param_count = param_count;
    if (param_names && param_count > 0) {
        entry->param_names = malloc(param_count * sizeof(char*));
        if (entry->param_names) {
            for (int i = 0; i < param_count; i++) {
                entry->param_names[i] = param_names[i] ? strdup(param_names[i]) : NULL;
            }
        }
    }
}

// Add to thread-local cache
static void cache_store(const char *input_sql, uint64_t hash, const char *output_sql,
                        int param_count, char **param_names) {
    int start_idx = (int)(hash & TRANS_CACHE_MASK);
    int oldest_idx = start_idx;
    
    for (int probe = 0; probe < 8; probe++) {
        int idx = (start_idx + probe) & TRANS_CACHE_MASK;
        trans_cache_entry_t *entry = &trans_cache[idx];
        
        if (entry->hash == 0) {
            // Empty slot - use it
            entry->hash = hash;
            entry->input_sql = strdup(input_sql);
            entry->output_sql = strdup(output_sql);
            cache_store_param_names(entry, param_names, param_count);
            return;
        }
        
        if (entry->hash == hash && entry->input_sql && strcmp(entry->input_sql, input_sql) == 0) {
            // Already exists - update it
            free(entry->output_sql);
            entry->output_sql = strdup(output_sql);
            cache_store_param_names(entry, param_names, param_count);
            return;
        }
        
        oldest_idx = idx;  // Keep track of last slot for eviction
    }
    
    // No free slot in probe range - evict oldest (last probed)
    trans_cache_entry_t *entry = &trans_cache[oldest_idx];
    free(entry->input_sql);
    free(entry->output_sql);
    cache_free_param_names(entry);
    entry->hash = hash;
    entry->input_sql = strdup(input_sql);
    entry->output_sql = strdup(output_sql);
    cache_store_param_names(entry, param_names, param_count);
}

// Standard translation: call function, swap result
// CRITICAL FIX: Free current before returning NULL to prevent memory leak
#define TRANSLATE(func) do { \
    temp = func(current); \
    if (!temp) { \
        free(current); \
        return NULL; \
    } \
    free(current); \
    current = temp; \
} while(0)

// Optimized replacement: skip str_replace_nocase entirely if pattern absent
#define TRANSLATE_REPLACE(old_str, new_str) do { \
    if (strcasestr(current, old_str)) { \
        temp = str_replace_nocase(current, old_str, new_str); \
        if (temp) { \
            free(current); \
            current = temp; \
        } \
    } \
} while(0)

// ============================================================================
// Main Function Translator (orchestrates all function translations)
// ============================================================================

char* sql_translate_functions(const char *sql) {
    if (!sql) return NULL;

    char *current = strdup(sql);
    if (!current) return NULL;

    char *temp;

    // Use TRANSLATE macro for functions that return NULL when no changes
    // Use TRANSLATE_REPLACE macro for simple string replacements (checks pattern first)

    // 0. FTS4 queries -> ILIKE queries
    TRANSLATE(translate_fts);

    // 0b. Convert SQLite NULL sorting to PostgreSQL NULLS LAST
    TRANSLATE(translate_null_sorting);

    // 0c. Remove DISTINCT when ORDER BY is present
    TRANSLATE(translate_distinct_orderby);

    // 0e. Simplify typeof fixup patterns
    TRANSLATE(simplify_typeof_fixup);

    // 0f. Fix duplicate assignments (UPDATE set a=1, a=2)
    TRANSLATE(fix_duplicate_assignments);

    // 1. iif() -> CASE WHEN
    TRANSLATE(translate_iif);

    // 2. typeof() -> pg_typeof()::text
    TRANSLATE(translate_typeof);

    // 3. strftime() -> EXTRACT/TO_CHAR
    TRANSLATE(translate_strftime);

    // 4. unixepoch() -> EXTRACT(EPOCH FROM ...)
    TRANSLATE(translate_unixepoch);

    // 5. datetime('now') -> NOW()
    TRANSLATE(translate_datetime);

    // 5a. last_insert_rowid() -> lastval()
    TRANSLATE(translate_last_insert_rowid);

    // 5b. json_each() -> json_array_elements()
    TRANSLATE(translate_json_each);

    // 5c. instr() -> STRPOS()
    TRANSLATE(translate_instr);

    // 6. IFNULL -> COALESCE (only if pattern exists)
    TRANSLATE_REPLACE("IFNULL(", "COALESCE(");

    // 7. SUBSTR -> SUBSTRING (only if pattern exists)
    TRANSLATE_REPLACE("SUBSTR(", "SUBSTRING(");

    // 11. max(a, b) -> GREATEST(a, b)
    TRANSLATE(translate_max_to_greatest);

    // 12. min(a, b) -> LEAST(a, b)
    TRANSLATE(translate_min_to_least);

    // 13. CASE THEN 0/1 -> THEN FALSE/TRUE
    TRANSLATE(translate_case_booleans);

    // 14. Add alias to subqueries in FROM clause
    TRANSLATE(add_subquery_alias);

    // 15. Fix forward reference in self-joins
    TRANSLATE(fix_forward_reference_joins);

    // 15a. Fix integer/text mismatch
    if (strcasestr(current, "download_queue_items")) {
        LOG_INFO("BEFORE fix_integer_text_mismatch: %.300s", current);
    }
    TRANSLATE(fix_integer_text_mismatch);
    if (strcasestr(current, "download_queue_items")) {
        LOG_INFO("AFTER fix_integer_text_mismatch: %.300s", current);
    }

    // 15b. Fix GROUP BY strict mode (legacy single-case handler)
    TRANSLATE(fix_group_by_strict);

    // 15b2. Fix GROUP BY strict mode (complete rewriter)
    TRANSLATE(fix_group_by_strict_complete);

    // 15b3. Fix clusters subquery AFTER group by rewriter (it incorrectly adds outer columns)
    TRANSLATE(fix_group_by_strict);
    
    // 15b4. Add ORDER BY NULLS FIRST for GROUP BY queries (SOCI compatibility)
    // This ensures NULL values come first in results, which helps SOCI detect nullable columns
    TRANSLATE(add_nulls_first_ordering);

    // 15c. Strip "collate icu_root"
    TRANSLATE(strip_icu_collation);

    // 15c2. Translate COLLATE NOCASE to LOWER()
    TRANSLATE(translate_collate_nocase);

    // 15d. Fix JSON operator ->> on TEXT columns
    TRANSLATE(fix_json_operator_on_text);

    // 15e. IFNULL -> COALESCE (PostgreSQL uses COALESCE)
    if (strcasestr(current, "IFNULL")) {
        char *temp = str_replace_nocase(current, "IFNULL", "COALESCE");
        if (temp && temp != current) {
            free(current);
            current = temp;
        }
    }

    // 16. Fix incomplete GROUP BY for specific queries - this runs BEFORE fix_group_by_strict_complete
    // so we can't rely on the full GROUP BY clause being present yet
    // Just do nothing here and let fix_group_by_strict_complete handle it

    // Fix for metadata_item_views query with max(viewed_at) - must run AFTER GROUP BY fix
    // This handles the case where ORDER BY uses a column that appears in an aggregate
    if (strcasestr(current, "max(viewed_at") && strcasestr(current, "order by viewed_at")) {
        // Replace "order by viewed_at" with "order by max(viewed_at)"
        temp = str_replace_nocase(current, "order by viewed_at desc", "order by max(viewed_at) desc");
        if (!temp) {
            temp = str_replace_nocase(current, "order by viewed_at", "order by max(viewed_at)");
        }
        if (temp) {
            free(current);
            current = temp;
        }
    }

    // external_metadata_items query fix
    if (strcasestr(current, "external_metadata_items.id,uri,user_title") &&
        strcasestr(current, "group by title order by")) {
        temp = str_replace(current,
            "group by title order by",
            "group by title,external_metadata_items.id,uri,user_title,library_section_id,metadata_type,year,added_at,updated_at,extra_data order by");
        free(current);
        if (!temp) { return NULL; }
        current = temp;
    }

    // metadata_item_clusterings fix
    if (strcasestr(current, "metadata_item_clusterings") &&
        strcasestr(current, "group by")) {
        if (strcasestr(current, "select DISTINCT")) {
            char *group_pos = strcasestr(current, " group by ");
            if (group_pos) {
                char *end = strcasestr(group_pos + 10, " order by ");
                if (!end) end = strcasestr(group_pos + 10, " limit ");
                if (!end) end = group_pos + strlen(group_pos);

                size_t before_len = group_pos - current;
                size_t after_len = strlen(end);
                char *new_sql = malloc(before_len + after_len + 1);
                if (new_sql) {
                    memcpy(new_sql, current, before_len);
                    memcpy(new_sql + before_len, end, after_len);
                    new_sql[before_len + after_len] = '\0';
                    free(current);
                    current = new_sql;
                }
            }
        }
    }

    // Final pass: Fix any remaining integer/text mismatches after all translations
    // This catches json_array_elements patterns that were just created
    if (strcasestr(current, "json_array_elements")) {
        LOG_INFO("Final pass: checking json_array_elements for type mismatches");
        temp = fix_integer_text_mismatch(current);
        if (temp) {
            free(current);
            current = temp;
        }
    }

    return current;
}

// ============================================================================
// Initialization and Cleanup
// ============================================================================

void sql_translator_init(void) {
    // Nothing to initialize for now
}

void sql_translator_cleanup(void) {
    // Nothing to cleanup for now
}

// ============================================================================
// Main Translation Function
// ============================================================================

sql_translation_t sql_translate(const char *sqlite_sql) {
    sql_translation_t result = {0};

    if (!sqlite_sql) {
        strcpy(result.error, "NULL input SQL");
        return result;
    }

    // Check thread-local cache first (lock-free, ~500x faster than translation)
    uint64_t hash = hash_sql(sqlite_sql);
    sql_translation_t *cached = cache_lookup(sqlite_sql, hash);
    if (cached) {
        // Cache hit - return copy of cached result
        result.sql = strdup(cached->sql);  // Caller expects to free this
        result.param_count = cached->param_count;
        // Deep-copy param_names so caller can free them via sql_translation_free()
        if (cached->param_names && cached->param_count > 0) {
            result.param_names = malloc(cached->param_count * sizeof(char*));
            if (result.param_names) {
                for (int i = 0; i < cached->param_count; i++) {
                    result.param_names[i] = cached->param_names[i] ? strdup(cached->param_names[i]) : NULL;
                }
            }
        } else {
            result.param_names = NULL;
        }
        result.success = 1;
        return result;
    }

    // Cache miss - do full translation

    // Step 1: Translate placeholders
    char *step1 = sql_translate_placeholders(sqlite_sql, &result.param_names, &result.param_count);
    if (!step1) {
        strcpy(result.error, "Placeholder translation failed");
        return result;
    }

    // Step 2: Translate functions
    char *step2 = sql_translate_functions(step1);
    free(step1);
    if (!step2) {
        strcpy(result.error, "Function translation failed");
        return result;
    }

    // Step 3: Translate types
    char *step3 = sql_translate_types(step2);
    free(step2);
    if (!step3) {
        strcpy(result.error, "Type translation failed");
        return result;
    }

    // Step 4: Translate keywords
    char *step4 = sql_translate_keywords(step3);
    free(step3);
    if (!step4) {
        strcpy(result.error, "Keyword translation failed");
        return result;
    }

    // Step 4a: Translate INSERT OR REPLACE to ON CONFLICT DO UPDATE
    char *step4a = translate_insert_or_replace(step4);
    free(step4);
    if (!step4a) {
        strcpy(result.error, "INSERT OR REPLACE translation failed");
        return result;
    }

    // Step 5: Translate DDL quotes
    char *step5 = translate_ddl_quotes(step4a);
    free(step4a);
    if (!step5) {
        strcpy(result.error, "DDL quote translation failed");
        return result;
    }

    // Step 6: Add IF NOT EXISTS
    char *step6 = add_if_not_exists(step5);
    free(step5);
    if (!step6) {
        strcpy(result.error, "IF NOT EXISTS translation failed");
        return result;
    }

    // Step 7: Fix operator spacing
    char *step7 = fix_operator_spacing(step6);
    free(step6);
    if (!step7) {
        strcpy(result.error, "Operator spacing fix failed");
        return result;
    }

    // Step 8: Fix ON CONFLICT quotes
    char *step8 = fix_on_conflict_quotes(step7);
    free(step7);
    if (!step8) {
        strcpy(result.error, "ON CONFLICT quote fix failed");
        return result;
    }

    // Step 9: Fix collections query (add metadata_type to SELECT)
    char *step9 = fix_collections_query(step8);
    free(step8);
    if (!step9) {
        strcpy(result.error, "Collections query fix failed");
        return result;
    }

    // Step 10: Quote mixed-case identifiers after AS (PostgreSQL lowercases unquoted)
    char *step10 = quote_mixed_case_identifiers(step9);
    free(step9);
    if (!step10) {
        strcpy(result.error, "Mixed-case identifier quoting failed");
        return result;
    }

    result.sql = step10;
    result.success = 1;

    // Store in thread-local cache for future lookups
    cache_store(sqlite_sql, hash, step10, result.param_count, result.param_names);

    return result;
}

// ============================================================================
// Cleanup Functions
// ============================================================================

void sql_translation_free(sql_translation_t *result) {
    if (!result) return;

    if (result->sql) {
        free(result->sql);
        result->sql = NULL;
    }

    if (result->param_names) {
        for (int i = 0; i < result->param_count; i++) {
            if (result->param_names[i]) {
                free(result->param_names[i]);
            }
        }
        free(result->param_names);
        result->param_names = NULL;
    }

    result->param_count = 0;
    result->success = 0;
}

void sql_translator_free(char *sql) {
    free(sql);
}
