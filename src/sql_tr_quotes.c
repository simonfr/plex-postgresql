/*
 * SQL Translator - Quote/Identifier Translations
 * Converts SQLite identifier quoting to PostgreSQL style
 */

#include "sql_translator_internal.h"

// ============================================================================
// Translate backticks `column` -> "column"
// ============================================================================

char* translate_backticks(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;

    while (*p) {
        if (*p == '`') {
            *out++ = '"';
        } else {
            *out++ = *p;
        }
        p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Translate table.'column' -> table."column"
// ============================================================================

char* translate_column_quotes(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;

    while (*p) {
        // Check for table.'column' pattern
        if (*p == '\'' && p > sql && *(p-1) == '.') {
            *out++ = '"';
            p++;

            while (*p && *p != '\'') {
                *out++ = *p++;
            }

            if (*p == '\'') {
                *out++ = '"';
                p++;
            }
            continue;
        }

        // Track regular string literals
        if (*p == '\'' && !in_string) {
            in_string = 1;
            *out++ = *p++;
            continue;
        }
        if (*p == '\'' && in_string) {
            if (*(p+1) == '\'') {
                *out++ = *p++;
                *out++ = *p++;
                continue;
            }
            in_string = 0;
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Translate AS 'alias' -> AS "alias"
// ============================================================================

char* translate_alias_quotes(const char *sql) {
    if (!sql) return NULL;

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;
    char string_char = 0;

    while (*p) {
        if ((*p == '\'' || *p == '"') && !in_string) {
            const char *back = p - 1;
            while (back > sql && isspace(*back)) back--;

            // Check if preceded by AS
            if (back >= sql + 1 &&
                (back[-1] == 'a' || back[-1] == 'A') &&
                (back[0] == 's' || back[0] == 'S') &&
                (back == sql + 1 || !is_ident_char(back[-2]))) {

                if (*p == '\'') {
                    *out++ = '"';
                    p++;

                    while (*p && *p != '\'') {
                        *out++ = *p++;
                    }

                    if (*p == '\'') {
                        *out++ = '"';
                        p++;
                    }
                    continue;
                }
            }

            in_string = 1;
            string_char = *p;
            *out++ = *p++;
            continue;
        }

        if (in_string && *p == string_char) {
            if (*(p+1) == string_char) {
                *out++ = *p++;
                *out++ = *p++;
                continue;
            }
            in_string = 0;
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Translate DDL single-quoted identifiers -> double-quoted
// ============================================================================

char* translate_ddl_quotes(const char *sql) {
    if (!sql) return NULL;

    const char *s = sql;
    while (*s && isspace(*s)) s++;
    int is_ddl = (strncasecmp(s, "CREATE", 6) == 0 ||
                  strncasecmp(s, "DROP", 4) == 0 ||
                  strncasecmp(s, "ALTER", 5) == 0);

    if (!is_ddl) {
        return strdup(sql);
    }

    char *result = malloc(strlen(sql) * 2 + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_parens = 0;

    while (*p) {
        if (*p == '(') in_parens++;
        if (*p == ')') in_parens--;

        if (*p == '\'') {
            const char *back = p - 1;
            while (back > sql && isspace(*back)) back--;

            int is_identifier = 0;

            if (back >= sql) {
                if ((back >= sql + 4 && strncasecmp(back - 4, "TABLE", 5) == 0) ||
                    (back >= sql + 4 && strncasecmp(back - 4, "INDEX", 5) == 0) ||
                    (back >= sql + 1 && strncasecmp(back - 1, "ON", 2) == 0) ||
                    (back >= sql + 5 && strncasecmp(back - 5, "UNIQUE", 6) == 0) ||
                    (back >= sql + 2 && strncasecmp(back - 2, "ADD", 3) == 0) ||
                    (back >= sql + 5 && strncasecmp(back - 5, "COLUMN", 6) == 0) ||
                    (back >= sql + 3 && strncasecmp(back - 3, "DROP", 4) == 0) ||
                    *back == '(' || *back == ',' || *back == '.') {
                    is_identifier = 1;
                }
            }

            if (p > sql) {
                const char *keyword = sql;
                while (*keyword && isspace(*keyword)) keyword++;
                if ((strncasecmp(keyword, "CREATE TABLE ", 13) == 0 ||
                     strncasecmp(keyword, "CREATE INDEX ", 13) == 0 ||
                     strncasecmp(keyword, "CREATE UNIQUE INDEX ", 20) == 0) &&
                    p > keyword) {
                    const char *check = p - 1;
                    while (check > keyword && isspace(*check)) check--;
                    if (check > keyword && (
                        (check >= keyword + 4 && strncasecmp(check - 4, "TABLE", 5) == 0) ||
                        (check >= keyword + 4 && strncasecmp(check - 4, "INDEX", 5) == 0))) {
                        is_identifier = 1;
                    }
                }
            }

            if (in_parens > 0 && back >= sql && (*back == '(' || *back == ',')) {
                is_identifier = 1;
            }

            if (is_identifier) {
                *out++ = '"';
                p++;

                while (*p && *p != '\'') {
                    *out++ = *p++;
                }

                if (*p == '\'') {
                    *out++ = '"';
                    p++;
                }
                continue;
            }
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Add IF NOT EXISTS to CREATE TABLE/INDEX
// ============================================================================

char* add_if_not_exists(const char *sql) {
    if (!sql) return NULL;

    const char *s = sql;
    while (*s && isspace(*s)) s++;

    // CREATE TABLE
    if (strncasecmp(s, "CREATE TABLE ", 13) == 0 &&
        strncasecmp(s + 13, "IF NOT EXISTS ", 14) != 0) {
        size_t prefix_len = (s - sql) + 12;
        size_t rest_len = strlen(s + 12);
        char *result = malloc(prefix_len + 15 + rest_len + 1);
        if (!result) return NULL;

        memcpy(result, sql, prefix_len);
        memcpy(result + prefix_len, " IF NOT EXISTS", 14);
        strcpy(result + prefix_len + 14, s + 12);
        return result;
    }

    // CREATE INDEX
    if (strncasecmp(s, "CREATE INDEX ", 13) == 0 &&
        strncasecmp(s + 13, "IF NOT EXISTS ", 14) != 0) {
        size_t prefix_len = (s - sql) + 12;
        size_t rest_len = strlen(s + 12);
        char *result = malloc(prefix_len + 15 + rest_len + 1);
        if (!result) return NULL;

        memcpy(result, sql, prefix_len);
        memcpy(result + prefix_len, " IF NOT EXISTS", 14);
        strcpy(result + prefix_len + 14, s + 12);
        return result;
    }

    // CREATE UNIQUE INDEX
    if (strncasecmp(s, "CREATE UNIQUE INDEX ", 20) == 0 &&
        strncasecmp(s + 20, "IF NOT EXISTS ", 14) != 0) {
        size_t prefix_len = (s - sql) + 19;
        size_t rest_len = strlen(s + 19);
        char *result = malloc(prefix_len + 15 + rest_len + 1);
        if (!result) return NULL;

        memcpy(result, sql, prefix_len);
        memcpy(result + prefix_len, " IF NOT EXISTS", 14);
        strcpy(result + prefix_len + 14, s + 19);
        return result;
    }

    return strdup(sql);
}

// ============================================================================
// Fix ON CONFLICT quotes: ON CONFLICT("name") -> ON CONFLICT(name)
// PostgreSQL requires unquoted column names in ON CONFLICT clause
// ============================================================================

char* fix_on_conflict_quotes(const char *sql) {
    if (!sql) return NULL;

    // Quick check if there's even an ON CONFLICT clause
    const char *on_conflict = strcasestr(sql, "ON CONFLICT");
    if (!on_conflict) {
        return strdup(sql);
    }

    char *result = malloc(strlen(sql) + 1);
    if (!result) return NULL;

    char *out = result;
    const char *p = sql;
    int in_string = 0;
    char string_char = 0;
    int inside_on_conflict_parens = 0;
    int paren_depth = 0;

    while (*p) {
        // Track ON CONFLICT clause start
        if (!in_string && !inside_on_conflict_parens) {
            if (strncasecmp(p, "ON CONFLICT", 11) == 0) {
                // Copy "ON CONFLICT"
                memcpy(out, p, 11);
                out += 11;
                p += 11;

                // Skip whitespace
                while (*p && (*p == ' ' || *p == '\t' || *p == '\n')) {
                    *out++ = *p++;
                }

                // Check if opening paren follows
                if (*p == '(') {
                    *out++ = *p++;
                    inside_on_conflict_parens = 1;
                    paren_depth = 1;
                }
                continue;
            }
        }

        // Inside ON CONFLICT parentheses
        if (inside_on_conflict_parens && !in_string) {
            // Track paren depth
            if (*p == '(') {
                paren_depth++;
                *out++ = *p++;
                continue;
            } else if (*p == ')') {
                paren_depth--;
                *out++ = *p++;
                if (paren_depth == 0) {
                    inside_on_conflict_parens = 0;
                }
                continue;
            }

            // Remove quotes around identifiers
            if (*p == '"') {
                // Skip the opening quote
                p++;
                // Copy the identifier without quotes
                while (*p && *p != '"') {
                    *out++ = *p++;
                }
                // Skip the closing quote
                if (*p == '"') p++;
                continue;
            }
        }

        // Track string literals (single quotes)
        if (*p == '\'' && (p == sql || *(p-1) != '\\')) {
            if (!in_string) {
                in_string = 1;
                string_char = '\'';
            } else if (string_char == '\'') {
                // Check for escaped quotes
                if (*(p+1) == '\'') {
                    *out++ = *p++;
                    *out++ = *p++;
                    continue;
                }
                in_string = 0;
            }
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}

// ============================================================================
// Fix duplicate column assignments in UPDATE statements
// UPDATE table SET col=1, col=2 -> Keep only the last assignment
// ============================================================================

char* fix_duplicate_assignments(const char *sql) {
    if (!sql) return NULL;

    // Quick check - only applies to UPDATE statements
    const char *s = sql;
    while (*s && isspace(*s)) s++;
    if (strncasecmp(s, "UPDATE", 6) != 0) {
        return strdup(sql);
    }

    // Find the SET clause
    const char *set_pos = strcasestr(sql, " SET ");
    if (!set_pos) {
        return strdup(sql);
    }

    // Find WHERE clause (end of SET clause)
    const char *where_pos = strcasestr(set_pos, " WHERE ");
    const char *set_end = where_pos ? where_pos : (sql + strlen(sql));

    // Parse assignments in SET clause
    // NOTE: Using heap allocation instead of stack to prevent stack overflow
    // when called from deeply nested translation chain (38KB on stack was too much)
    #define MAX_ASSIGNMENTS 256
    typedef struct {
        char column[128];
        const char *col_start;    // Original column name start (including quotes)
        const char *col_end;      // Original column name end (including quotes)
        const char *value_start;
        const char *value_end;
        int keep;
    } assignment_t;

    assignment_t *assignments = calloc(MAX_ASSIGNMENTS, sizeof(assignment_t));
    if (!assignments) {
        return strdup(sql);
    }
    int assign_count = 0;

    const char *p = set_pos + 5;  // Skip " SET "
    while (p < set_end && assign_count < MAX_ASSIGNMENTS) {
        // Skip whitespace
        while (*p && isspace(*p) && p < set_end) p++;
        if (p >= set_end) break;

        // Extract column name (may be quoted with " or `)
        const char *col_start = p;
        if (*p == '"') {
            p++;
            while (*p && *p != '"' && p < set_end) p++;
            if (*p == '"') p++;
        } else if (*p == '`') {
            p++;
            while (*p && *p != '`' && p < set_end) p++;
            if (*p == '`') p++;
        } else {
            while (*p && !isspace(*p) && *p != '=' && p < set_end) p++;
        }
        const char *col_end = p;

        size_t col_len = col_end - col_start;
        if (col_len >= sizeof(assignments[0].column)) col_len = sizeof(assignments[0].column) - 1;

        // Normalize column name (remove quotes and backticks, lowercase)
        char column[128] = {0};
        const char *src = col_start;
        char *dst = column;
        while (src < col_start + col_len && dst < column + sizeof(column) - 1) {
            if (*src != '"' && *src != '`') {
                *dst++ = tolower(*src);
            }
            src++;
        }
        *dst = '\0';

        // Skip whitespace and '='
        while (*p && (isspace(*p) || *p == '=') && p < set_end) p++;

        // Find value (ends at ',' or WHERE)
        const char *value_start = p;
        int paren_depth = 0;
        int in_string = 0;
        while (p < set_end) {
            if (*p == '\'' && (p == sql || *(p-1) != '\\')) {
                in_string = !in_string;
            }
            if (!in_string) {
                if (*p == '(') paren_depth++;
                if (*p == ')') paren_depth--;
                if (*p == ',' && paren_depth == 0) break;
            }
            p++;
        }
        const char *value_end = p;

        // Store assignment
        strncpy(assignments[assign_count].column, column, sizeof(assignments[assign_count].column) - 1);
        assignments[assign_count].col_start = col_start;
        assignments[assign_count].col_end = col_end;
        assignments[assign_count].value_start = value_start;
        assignments[assign_count].value_end = value_end;
        assignments[assign_count].keep = 1;
        assign_count++;

        // Skip comma
        if (*p == ',') p++;
    }

    // Mark duplicates - keep only the LAST occurrence of each column
    for (int i = 0; i < assign_count; i++) {
        for (int j = i + 1; j < assign_count; j++) {
            if (strcmp(assignments[i].column, assignments[j].column) == 0) {
                assignments[i].keep = 0;  // Remove earlier occurrence
                break;
            }
        }
    }

    // Count how many assignments we're keeping and find removed $N params
    int keep_count = 0;
    int removed_params[256];  // Track which $N params were removed
    int removed_count = 0;
    memset(removed_params, 0, sizeof(removed_params));

    for (int i = 0; i < assign_count; i++) {
        if (assignments[i].keep) {
            keep_count++;
        } else {
            // Find $N params in the removed assignment's value
            const char *vp = assignments[i].value_start;
            while (vp < assignments[i].value_end) {
                if (*vp == '$' && vp + 1 < assignments[i].value_end && isdigit(*(vp+1))) {
                    int n = atoi(vp + 1);
                    if (n > 0 && n < 256) {
                        removed_params[n] = 1;
                        removed_count++;
                    }
                }
                vp++;
            }
        }
    }

    // If no duplicates, return original
    if (keep_count == assign_count) {
        free(assignments);
        return strdup(sql);
    }

    // Rebuild SQL with deduplicated assignments
    // Append removed params as harmless casts in WHERE clause to keep PG happy
    // e.g.: WHERE "id"=$3 AND $1::text IS NOT NULL
    size_t result_len = strlen(sql) * 2 + removed_count * 30 + 1;
    char *result = malloc(result_len);
    if (!result) {
        free(assignments);
        return strdup(sql);
    }

    // Copy up to SET clause
    size_t prefix_len = (set_pos + 5) - sql;
    memcpy(result, sql, prefix_len);
    char *out = result + prefix_len;

    // Copy kept assignments
    int first = 1;
    for (int i = 0; i < assign_count; i++) {
        if (!assignments[i].keep) continue;

        if (!first) {
            *out++ = ',';
            *out++ = ' ';
        }
        first = 0;

        // Copy column name from stored pointers
        size_t col_len = assignments[i].col_end - assignments[i].col_start;
        memcpy(out, assignments[i].col_start, col_len);
        out += col_len;

        // Copy = and value
        *out++ = '=';
        size_t value_len = assignments[i].value_end - assignments[i].value_start;
        memcpy(out, assignments[i].value_start, value_len);
        out += value_len;
    }

    // Copy rest of SQL (WHERE clause, etc.)
    if (where_pos) {
        strcpy(out, where_pos);
        out += strlen(where_pos);
    }

    // Consume removed params in a harmless way so PG can determine their types
    // This prevents "could not determine data type of parameter $N" errors
    // We use COALESCE($N::text, '') which evaluates the param but discards the result
    if (removed_count > 0) {
        // Append as a no-op condition: AND TRUE (with side-effect of referencing unused params)
        out += sprintf(out, " AND COALESCE(");
        int first_removed = 1;
        for (int n = 1; n < 256; n++) {
            if (removed_params[n]) {
                if (!first_removed) {
                    out += sprintf(out, ",");
                }
                first_removed = 0;
                out += sprintf(out, "$%d::text", n);
            }
        }
        out += sprintf(out, ",'') IS NOT NULL");
    }

    *out = '\0';

    free(assignments);
    return result;
}

// ============================================================================
// Quote mixed-case identifiers: both AS definitions and references
//
// PostgreSQL lowercases unquoted identifiers, so:
//   AS blankKeyTaggingId  -> AS "blankKeyTaggingId"
//   grandparentsSettings.col -> "grandparentsSettings".col
//
// Two-pass approach:
//   Pass 1: Find all AS <mixedCaseIdent> and collect identifier names
//   Pass 2: Quote all bare (unquoted) occurrences of those identifiers
// ============================================================================

// Helper: check if identifier has any uppercase letter
static int has_uppercase(const char *s, size_t len) {
    for (size_t i = 0; i < len; i++) {
        if (s[i] >= 'A' && s[i] <= 'Z') return 1;
    }
    return 0;
}

// Helper: skip over a string literal (single or double quoted)
// Returns pointer to character after closing quote
static const char* skip_string_literal(const char *p) {
    char quote = *p++;
    while (*p) {
        if (*p == quote) {
            if (*(p + 1) == quote) {
                p += 2;  // escaped quote
                continue;
            }
            return p + 1;  // past closing quote
        }
        p++;
    }
    return p;
}

#define MAX_MIXED_IDENTS 32

// SQL type keywords that appear after AS in CAST() — must NOT be quoted
static int is_sql_type_keyword(const char *s, size_t len) {
    // Common SQL type names (case-insensitive check)
    static const char *types[] = {
        "INTEGER", "INT", "BIGINT", "SMALLINT", "TINYINT",
        "TEXT", "VARCHAR", "CHAR", "CHARACTER",
        "REAL", "FLOAT", "DOUBLE", "NUMERIC", "DECIMAL",
        "BOOLEAN", "BOOL",
        "BLOB", "BYTEA",
        "DATE", "TIME", "TIMESTAMP", "TIMESTAMPTZ", "INTERVAL",
        "JSON", "JSONB",
        "UUID",
        NULL
    };
    for (int i = 0; types[i]; i++) {
        if (strlen(types[i]) == len && strncasecmp(s, types[i], len) == 0) {
            return 1;
        }
    }
    return 0;
}

char* quote_mixed_case_identifiers(const char *sql) {
    if (!sql) return NULL;

    // ---- Pass 1: Collect mixed-case identifiers after AS ----
    char idents[MAX_MIXED_IDENTS][128];
    int ident_count = 0;

    const char *p = sql;
    while (*p) {
        // Skip string literals
        if (*p == '\'' || *p == '"') {
            p = skip_string_literal(p);
            continue;
        }

        // Look for AS keyword
        if ((*p == 'a' || *p == 'A') &&
            (*(p + 1) == 's' || *(p + 1) == 'S') &&
            (p == sql || !is_ident_char(*(p - 1))) &&
            *(p + 2) && !is_ident_char(*(p + 2))) {

            p += 2;  // skip "AS"
            while (*p && (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r')) p++;

            // Already quoted or subquery - skip
            if (*p == '"' || *p == '\'' || *p == '(') continue;

            // Extract identifier
            const char *id_start = p;
            while (*p && is_ident_char(*p)) p++;
            size_t id_len = p - id_start;

            if (id_len > 0 && id_len < 128 && has_uppercase(id_start, id_len) &&
                !is_sql_type_keyword(id_start, id_len) &&
                ident_count < MAX_MIXED_IDENTS) {
                // Check for duplicates
                int dup = 0;
                for (int i = 0; i < ident_count; i++) {
                    if (strlen(idents[i]) == id_len && memcmp(idents[i], id_start, id_len) == 0) {
                        dup = 1;
                        break;
                    }
                }
                if (!dup) {
                    memcpy(idents[ident_count], id_start, id_len);
                    idents[ident_count][id_len] = '\0';
                    ident_count++;
                }
            }
            continue;
        }

        p++;
    }

    // No mixed-case identifiers found - return as-is
    if (ident_count == 0) {
        return strdup(sql);
    }

    // ---- Pass 2: Quote all bare occurrences of collected identifiers ----
    size_t len = strlen(sql);
    // Each identifier occurrence could grow by 2 chars (quotes), allocate generously
    char *result = malloc(len * 3 + 1);
    if (!result) return NULL;

    char *out = result;
    p = sql;

    while (*p) {
        // Skip string literals (single-quoted)
        if (*p == '\'') {
            const char *end = skip_string_literal(p);
            memcpy(out, p, end - p);
            out += (end - p);
            p = end;
            continue;
        }

        // Skip already double-quoted identifiers
        if (*p == '"') {
            const char *end = skip_string_literal(p);
            memcpy(out, p, end - p);
            out += (end - p);
            p = end;
            continue;
        }

        // Check if current position starts an identifier character
        if (is_ident_char(*p)) {
            // Extract the full identifier at this position
            const char *id_start = p;
            while (*p && is_ident_char(*p)) p++;
            size_t id_len = p - id_start;

            // Check if this identifier matches any of our collected mixed-case names
            int matched = 0;
            for (int i = 0; i < ident_count; i++) {
                size_t idn_len = strlen(idents[i]);
                if (id_len == idn_len && memcmp(id_start, idents[i], id_len) == 0) {
                    // Must not be preceded by identifier char (already guaranteed by how we got here)
                    // and must not be followed by identifier char (already guaranteed)
                    *out++ = '"';
                    memcpy(out, id_start, id_len);
                    out += id_len;
                    *out++ = '"';
                    matched = 1;
                    break;
                }
            }

            if (!matched) {
                // Copy identifier as-is
                memcpy(out, id_start, id_len);
                out += id_len;
            }
            continue;
        }

        *out++ = *p++;
    }

    *out = '\0';
    return result;
}
