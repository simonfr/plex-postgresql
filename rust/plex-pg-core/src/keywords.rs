/// Module: keywords
///
/// Rewrites SQLite-specific keywords/constructs to PostgreSQL equivalents:
///   BEGIN IMMEDIATE / DEFERRED / EXCLUSIVE → plain BEGIN
///   GLOB '*foo*'        → ILIKE '%foo%'  (via pre-parse string normalisation)
///   IN ()               → IN (SELECT -1 WHERE FALSE)
///   GROUP BY NULL       → remove GROUP BY
///   sqlite_master / sqlite_schema → information_schema subquery
///   INDEXED BY idx      → removed  (via pre-parse string normalisation)
///   ORDER BY rowid      → removed when querying sqlite_master
use sqlparser::ast::helpers::attached_token::AttachedToken;
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::atomic::AtomicI8;

const STRICT_PRAGMA_ERROR_SQL: &str = "__STRICT_PRAGMA_ERROR__";
static PRAGMA_TOTAL: AtomicU64 = AtomicU64::new(0);
static PRAGMA_MAPPED_SET: AtomicU64 = AtomicU64::new(0);
static PRAGMA_MAPPED_READ: AtomicU64 = AtomicU64::new(0);
static PRAGMA_NOOP_SET: AtomicU64 = AtomicU64::new(0);
static PRAGMA_STRIPPED: AtomicU64 = AtomicU64::new(0);
static PRAGMA_STRICT_FAIL: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_STRICT_PRAGMA_OVERRIDE: AtomicI8 = AtomicI8::new(-1);

pub fn transform(stmt: &mut Statement) {
    match stmt {
        // BEGIN IMMEDIATE / DEFERRED / EXCLUSIVE → plain BEGIN
        Statement::StartTransaction { modifier, .. } => {
            *modifier = None;
        }

        // SELECT / subquery rewrites
        Statement::Query(q) => transform_query(q),
        Statement::Insert(ins) => {
            if let Some(src) = &mut ins.source {
                transform_query(src);
            }
        }
        Statement::Update(u) => {
            if let Some(sel) = &mut u.selection {
                transform_expr(sel);
            }
        }
        Statement::Delete(d) => {
            if let Some(sel) = &mut d.selection {
                transform_expr(sel);
            }
        }
        _ => {}
    }
}

// ─── Pre-parse string normalisation helpers ───────────────────────────────────

/// Normalise raw SQL before parsing:
///   - `GLOB 'pattern'` → `ILIKE 'pg_pattern'`  (wildcards: * → %, ? → _)
///   - `INDEXED BY <name>` → removed
///   - `COLLATE icu_*` / `COLLATE UNICODE[_CI]` / `COLLATE RTRIM` / `COLLATE BINARY` → removed
///     (`COLLATE NOCASE` is left for the AST-level handler in query.rs which converts it
///      to LOWER(…))
///
/// Called from `crate::preprocess` (lib.rs) before the SQL hits sqlparser.
pub fn preprocess(sql: &str) -> String {
    let sql = fix_placeholder_spacing(sql);
    let sql = rewrite_sqlite_utility_statements(&sql);
    let sql = rewrite_regexp_operator(&sql);
    let sql = rewrite_raise_function_calls(&sql);
    let sql = rewrite_or_conflict_prefix_statements(&sql);
    let sql = rewrite_limit_offset_comma_syntax(&sql);
    let sql = rewrite_update_delete_limit_statements(&sql);
    let sql = rewrite_transaction_control_statements(&sql);
    let sql = rewrite_pragma_statements(&sql);
    let sql = rewrite_sqlite_create_table_options(&sql);
    let sql = rewrite_virtual_tables(&sql);
    let sql = rewrite_glob(&sql);
    let sql = rewrite_indexed_by(&sql);
    let sql = rewrite_sqlite_collations(&sql);
    let sql = rewrite_distinct_orderby_projection(&sql);
    rewrite_metadata_items_self_join(&sql)
}

/// Normalize SQLite conflict-resolution statement prefixes that PostgreSQL
/// does not support directly.
///
/// - `UPDATE OR <algo> ...` -> `UPDATE ...`
/// - `INSERT OR ABORT/FAIL/ROLLBACK INTO ...` -> `INSERT INTO ...`
///
/// `INSERT OR REPLACE/IGNORE` are intentionally left intact for `upsert` rewrite.
fn rewrite_or_conflict_prefix_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();

        // UPDATE OR <algo> ...
        if lower.starts_with("update or ") {
            let tail = t["update or ".len()..].trim_start();
            let algos = ["ignore", "replace", "rollback", "abort", "fail"];
            let mut matched = false;
            for algo in algos {
                if tail.to_ascii_lowercase().starts_with(algo) {
                    let rest = tail[algo.len()..].trim_start();
                    out.push(format!("UPDATE {}", rest));
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
        }

        // INSERT OR ABORT/FAIL/ROLLBACK INTO ...
        if lower.starts_with("insert or abort into ")
            || lower.starts_with("insert or fail into ")
            || lower.starts_with("insert or rollback into ")
        {
            if let Some((_, rest)) = t.split_once("INTO ") {
                out.push(format!("INSERT INTO {}", rest.trim_start()));
                continue;
            }
            if let Some((_, rest)) = t.split_once("into ") {
                out.push(format!("INSERT INTO {}", rest.trim_start()));
                continue;
            }
        }

        out.push(t.to_string());
    }
    out.join("; ")
}

/// Rewrite SQLite `UPDATE/DELETE ... [ORDER BY ...] LIMIT ...` forms to
/// PostgreSQL-compatible CTE+ctid targeting.
fn rewrite_update_delete_limit_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(r) = rewrite_single_update_limit_stmt(t) {
            out.push(r);
            continue;
        }
        if let Some(r) = rewrite_single_delete_limit_stmt(t) {
            out.push(r);
            continue;
        }
        out.push(t.to_string());
    }
    out.join("; ")
}

fn rewrite_single_update_limit_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    if !lower.starts_with("update ") {
        return None;
    }

    let after_update = "update ".len();
    let set_pos = find_top_level_keyword_from(stmt, after_update, "set")?;
    let table_part = stmt[after_update..set_pos].trim();
    if table_part.is_empty() {
        return None;
    }

    let after_set = set_pos + "set".len();
    let where_pos = find_top_level_keyword_from(stmt, after_set, "where");
    let order_pos = find_top_level_keyword_from(stmt, after_set, "order by");
    let limit_pos = find_top_level_keyword_from(stmt, after_set, "limit")?;
    let returning_pos = find_top_level_keyword_from(stmt, after_set, "returning");

    let set_end = [where_pos, order_pos, Some(limit_pos), returning_pos]
        .into_iter()
        .flatten()
        .filter(|p| *p > after_set)
        .min()
        .unwrap_or(stmt.len());
    let set_part = stmt[after_set..set_end].trim();
    if set_part.is_empty() {
        return None;
    }

    let where_part = where_pos.and_then(|wp| {
        if wp >= limit_pos {
            return None;
        }
        let where_end = [order_pos, Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > wp)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[wp + "where".len()..where_end].trim().to_string())
    });

    let order_part = order_pos.and_then(|op| {
        if op >= limit_pos {
            return None;
        }
        let order_end = [Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > op)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[op + "order by".len()..order_end].trim().to_string())
    });

    let limit_end = returning_pos.unwrap_or(stmt.len());
    if limit_end <= limit_pos + "limit".len() {
        return None;
    }
    let limit_part = stmt[limit_pos + "limit".len()..limit_end].trim();
    if limit_part.is_empty() {
        return None;
    }

    let returning_tail = returning_pos
        .map(|rp| stmt[rp..].trim().to_string())
        .unwrap_or_default();

    let mut target_sel = format!("SELECT ctid FROM {}", table_part);
    if let Some(w) = where_part.as_deref().filter(|w| !w.is_empty()) {
        target_sel.push_str(" WHERE ");
        target_sel.push_str(w);
    }
    if let Some(o) = order_part.as_deref().filter(|o| !o.is_empty()) {
        target_sel.push_str(" ORDER BY ");
        target_sel.push_str(o);
    }
    target_sel.push_str(" LIMIT ");
    target_sel.push_str(limit_part);

    let mut out = format!(
        "WITH _plex_target AS ({}) UPDATE {} SET {} WHERE ctid IN (SELECT ctid FROM _plex_target)",
        target_sel, table_part, set_part
    );
    if !returning_tail.is_empty() {
        out.push(' ');
        out.push_str(&returning_tail);
    }
    Some(out)
}

fn rewrite_single_delete_limit_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    if !lower.starts_with("delete from ") {
        return None;
    }

    let after_delete_from = "delete from ".len();
    let where_pos = find_top_level_keyword_from(stmt, after_delete_from, "where");
    let order_pos = find_top_level_keyword_from(stmt, after_delete_from, "order by");
    let limit_pos = find_top_level_keyword_from(stmt, after_delete_from, "limit")?;
    let returning_pos = find_top_level_keyword_from(stmt, after_delete_from, "returning");

    let table_end = [where_pos, order_pos, Some(limit_pos), returning_pos]
        .into_iter()
        .flatten()
        .filter(|p| *p > after_delete_from)
        .min()
        .unwrap_or(stmt.len());
    let table_part = stmt[after_delete_from..table_end].trim();
    if table_part.is_empty() {
        return None;
    }

    let where_part = where_pos.and_then(|wp| {
        if wp >= limit_pos {
            return None;
        }
        let where_end = [order_pos, Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > wp)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[wp + "where".len()..where_end].trim().to_string())
    });

    let order_part = order_pos.and_then(|op| {
        if op >= limit_pos {
            return None;
        }
        let order_end = [Some(limit_pos), returning_pos]
            .into_iter()
            .flatten()
            .filter(|p| *p > op)
            .min()
            .unwrap_or(stmt.len());
        Some(stmt[op + "order by".len()..order_end].trim().to_string())
    });

    let limit_end = returning_pos.unwrap_or(stmt.len());
    if limit_end <= limit_pos + "limit".len() {
        return None;
    }
    let limit_part = stmt[limit_pos + "limit".len()..limit_end].trim();
    if limit_part.is_empty() {
        return None;
    }

    let returning_tail = returning_pos
        .map(|rp| stmt[rp..].trim().to_string())
        .unwrap_or_default();

    let mut target_sel = format!("SELECT ctid FROM {}", table_part);
    if let Some(w) = where_part.as_deref().filter(|w| !w.is_empty()) {
        target_sel.push_str(" WHERE ");
        target_sel.push_str(w);
    }
    if let Some(o) = order_part.as_deref().filter(|o| !o.is_empty()) {
        target_sel.push_str(" ORDER BY ");
        target_sel.push_str(o);
    }
    target_sel.push_str(" LIMIT ");
    target_sel.push_str(limit_part);

    let mut out = format!(
        "WITH _plex_target AS ({}) DELETE FROM {} WHERE ctid IN (SELECT ctid FROM _plex_target)",
        target_sel, table_part
    );
    if !returning_tail.is_empty() {
        out.push(' ');
        out.push_str(&returning_tail);
    }
    Some(out)
}

/// Rewrite SQLite `LIMIT <offset>, <count>` to PostgreSQL `LIMIT <count> OFFSET <offset>`.
fn rewrite_limit_offset_comma_syntax(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        out.push(rewrite_single_limit_offset_comma_stmt(t));
    }
    out.join("; ")
}

fn rewrite_single_limit_offset_comma_stmt(stmt: &str) -> String {
    let Some(limit_start) = find_top_level_keyword(stmt, "limit") else {
        return stmt.to_string();
    };
    let limit_kw_end = limit_start + "limit".len();

    let mut i = limit_kw_end;
    let bytes = stmt.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return stmt.to_string();
    }

    if find_top_level_keyword_from(stmt, i, "offset").is_some() {
        return stmt.to_string();
    }

    let clause_end = find_top_level_keyword_after_any(stmt, i, &["fetch", "union", "except", "intersect"])
        .unwrap_or(stmt.len());

    let mut j = i;
    let mut depth = 0usize;
    let mut comma_idx: Option<usize> = None;
    while j < clause_end {
        match bytes[j] {
            b'\'' => {
                j += 1;
                while j < clause_end {
                    if bytes[j] == b'\'' {
                        j += 1;
                        if j < clause_end && bytes[j] == b'\'' {
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    j += 1;
                }
            }
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                j += 1;
            }
            b',' if depth == 0 => {
                comma_idx = Some(j);
                break;
            }
            _ => j += 1,
        }
    }

    let Some(comma) = comma_idx else {
        return stmt.to_string();
    };

    let offset_expr = stmt[i..comma].trim();
    let count_expr = stmt[comma + 1..clause_end].trim();
    if offset_expr.is_empty() || count_expr.is_empty() {
        return stmt.to_string();
    }

    let mut rewritten = String::with_capacity(stmt.len() + 16);
    rewritten.push_str(&stmt[..limit_start]);
    rewritten.push_str("LIMIT ");
    rewritten.push_str(count_expr);
    rewritten.push_str(" OFFSET ");
    rewritten.push_str(offset_expr);
    if clause_end < stmt.len() {
        rewritten.push_str(&stmt[clause_end..]);
    }
    rewritten
}

fn find_top_level_keyword(stmt: &str, keyword: &str) -> Option<usize> {
    find_top_level_keyword_from(stmt, 0, keyword)
}

fn find_top_level_keyword_from(stmt: &str, start: usize, keyword: &str) -> Option<usize> {
    if start >= stmt.len() {
        return None;
    }
    let bytes = stmt.as_bytes();
    let kw = keyword.as_bytes();
    let mut i = start;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'"' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'`' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'`' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'`' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0 => {
                if i + kw.len() <= bytes.len()
                    && stmt[i..i + kw.len()].eq_ignore_ascii_case(keyword)
                    && (i == 0 || !is_ident_char(bytes[i - 1]))
                    && (i + kw.len() == bytes.len() || !is_ident_char(bytes[i + kw.len()]))
                {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

fn find_top_level_keyword_after_any(stmt: &str, start: usize, keywords: &[&str]) -> Option<usize> {
    keywords
        .iter()
        .filter_map(|kw| find_top_level_keyword_from(stmt, start, kw))
        .min()
}

/// Normalize SQLite utility statements that are unsupported or differ in PostgreSQL.
///
/// - `EXPLAIN QUERY PLAN <stmt>` -> `EXPLAIN <stmt>`
/// - `VACUUM` / `REINDEX` -> `SELECT 1` (safe no-op compatibility)
/// - `ANALYZE sqlite_*` -> `SELECT 1` (SQLite-internal analyze target)
/// - `ATTACH/DETACH DATABASE` -> `SELECT 1` (safe no-op compatibility)
fn rewrite_sqlite_utility_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("explain query plan ") {
            let prefix_len = "explain query plan ".len();
            out.push(format!("EXPLAIN {}", t[prefix_len..].trim()));
            continue;
        }
        if lower == "vacuum" || lower.starts_with("vacuum ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower == "reindex" || lower.starts_with("reindex ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower.starts_with("attach database ") || lower.starts_with("detach database ") {
            out.push("SELECT 1".to_string());
            continue;
        }
        if lower.starts_with("analyze sqlite_")
            || lower.starts_with("analyze main.sqlite_")
            || lower.starts_with("analyze temp.sqlite_")
        {
            out.push("SELECT 1".to_string());
            continue;
        }
        out.push(t.to_string());
    }
    out.join("; ")
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn starts_with_ascii_icase_at(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    if at + needle.len() > haystack.len() {
        return false;
    }
    haystack[at..at + needle.len()]
        .iter()
        .zip(needle.iter())
        .all(|(a, n)| a.eq_ignore_ascii_case(n))
}

/// Rewrite SQLite `REGEXP` / `NOT REGEXP` operators to PostgreSQL `~` / `!~`.
/// This is a lexical rewrite outside of quoted strings.
fn rewrite_regexp_operator(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    let not_regexp = b"not regexp";
    let regexp = b"regexp";

    while i < bytes.len() {
        // Keep quoted strings untouched
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        let prev_is_ident = i > 0 && is_ident_char(bytes[i - 1]);
        if !prev_is_ident && starts_with_ascii_icase_at(bytes, i, not_regexp) {
            let end = i + not_regexp.len();
            let next_is_ident = end < bytes.len() && is_ident_char(bytes[end]);
            if !next_is_ident {
                out.push_str("!~");
                i = end;
                continue;
            }
        }
        if !prev_is_ident && starts_with_ascii_icase_at(bytes, i, regexp) {
            let end = i + regexp.len();
            let next_is_ident = end < bytes.len() && is_ident_char(bytes[end]);
            if !next_is_ident {
                out.push('~');
                i = end;
                continue;
            }
        }

        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Rewrite SQLite trigger-only `RAISE(...)` calls to `NULL` so parsing remains
/// PostgreSQL-compatible when trigger bodies are translated.
fn rewrite_raise_function_calls(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        if starts_with_ascii_icase_at(bytes, i, b"raise(") {
            let mut j = i + "raise(".len();
            let mut depth = 1usize;
            while j < bytes.len() {
                if bytes[j] == b'\'' {
                    j += 1;
                    while j < bytes.len() {
                        if bytes[j] == b'\'' {
                            j += 1;
                            if j < bytes.len() && bytes[j] == b'\'' {
                                j += 1;
                                continue;
                            }
                            break;
                        }
                        j += 1;
                    }
                    continue;
                }
                if bytes[j] == b'(' {
                    depth += 1;
                } else if bytes[j] == b')' {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                j += 1;
            }
            out.push_str("NULL");
            i = j;
            continue;
        }

        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Strip SQLite-only table options from CREATE TABLE statements:
/// - WITHOUT ROWID
/// - STRICT
///   including combined forms like `... ) WITHOUT ROWID, STRICT`.
fn rewrite_sqlite_create_table_options(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if !lower.starts_with("create table ") {
            out.push(t.to_string());
            continue;
        }
        let no_conflict = strip_create_table_on_conflict_clauses(t);
        out.push(strip_create_table_tail_options(&no_conflict));
    }
    out.join("; ")
}

fn strip_create_table_on_conflict_clauses(stmt: &str) -> String {
    let bytes = stmt.as_bytes();
    let mut out = String::with_capacity(stmt.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }

        if starts_with_ascii_icase_at(bytes, i, b"on conflict") {
            let prev_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            if prev_ok {
                let mut j = i + "on conflict".len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let rest = &stmt[j..].to_ascii_lowercase();
                let actions = ["rollback", "abort", "fail", "ignore", "replace"];
                let mut matched = false;
                for action in actions {
                    if rest.starts_with(action) {
                        j += action.len();
                        matched = true;
                        break;
                    }
                }
                if matched {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }

        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn strip_create_table_tail_options(stmt: &str) -> String {
    let mut s = stmt.trim().to_string();
    loop {
        let lower = s.to_ascii_lowercase();
        let trimmed = lower.trim_end();
        if trimmed.ends_with(", strict") {
            s.truncate(s.trim_end().len() - ", strict".len());
            s = s.trim_end().to_string();
            continue;
        }
        if trimmed.ends_with(", without rowid") {
            s.truncate(s.trim_end().len() - ", without rowid".len());
            s = s.trim_end().to_string();
            continue;
        }
        if trimmed.ends_with("strict") {
            s.truncate(s.trim_end().len() - "strict".len());
            s = s.trim_end().trim_end_matches(',').trim_end().to_string();
            continue;
        }
        if trimmed.ends_with("without rowid") {
            s.truncate(s.trim_end().len() - "without rowid".len());
            s = s.trim_end().trim_end_matches(',').trim_end().to_string();
            continue;
        }
        break;
    }
    s
}

/// Normalize SQLite transaction/control statement variants to PostgreSQL-friendly forms.
///
/// Examples:
/// - `BEGIN IMMEDIATE` / `BEGIN EXCLUSIVE` / `BEGIN TRANSACTION` -> `BEGIN`
/// - `END` / `END TRANSACTION` -> `COMMIT`
/// - `ROLLBACK TRANSACTION TO x` / `ROLLBACK TO x` -> `ROLLBACK TO SAVEPOINT x`
/// - `RELEASE x` -> `RELEASE SAVEPOINT x`
fn rewrite_transaction_control_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();

        let rewritten = if lower.starts_with("begin immediate")
            || lower.starts_with("begin deferred")
            || lower.starts_with("begin exclusive")
            || lower.starts_with("begin transaction")
            || lower == "begin"
        {
            "BEGIN".to_string()
        } else if lower == "end"
            || lower.starts_with("end transaction")
            || lower.starts_with("commit transaction")
        {
            "COMMIT".to_string()
        } else if let Some((_, rest)) = lower.split_once("rollback transaction to savepoint ") {
            format!(
                "ROLLBACK TO SAVEPOINT {}",
                t[t.len() - rest.len()..].trim()
            )
        } else if let Some((_, rest)) = lower.split_once("rollback transaction to ") {
            format!(
                "ROLLBACK TO SAVEPOINT {}",
                t[t.len() - rest.len()..].trim()
            )
        } else if let Some((_, rest)) = lower.split_once("rollback to savepoint ") {
            format!(
                "ROLLBACK TO SAVEPOINT {}",
                t[t.len() - rest.len()..].trim()
            )
        } else if let Some((_, rest)) = lower.split_once("rollback to ") {
            format!(
                "ROLLBACK TO SAVEPOINT {}",
                t[t.len() - rest.len()..].trim()
            )
        } else if let Some((_, rest)) = lower.split_once("release savepoint ") {
            format!("RELEASE SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("release ") {
            format!("RELEASE SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else if let Some((_, rest)) = lower.split_once("savepoint ") {
            format!("SAVEPOINT {}", t[t.len() - rest.len()..].trim())
        } else {
            t.to_string()
        };

        out.push(rewritten);
    }
    out.join("; ")
}

// Temporary compatibility shim: keep preprocess pipeline stable while
// DISTINCT/ORDER BY projection rewrite is disabled on this branch.
fn rewrite_distinct_orderby_projection(sql: &str) -> String {
    sql.to_string()
}

/// Rewrite common PRAGMA statements to safe PostgreSQL-compatible no-op/select forms.
///
/// This keeps more behavior stable than dropping all PRAGMAs:
/// - assignment-like pragmas become `SELECT 1`
/// - read-like pragmas become a single-row `SELECT ... AS <pragma_name>`
///   Unknown pragmas are still stripped.
fn rewrite_pragma_statements(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.len() >= 6 && t[..6].eq_ignore_ascii_case("pragma") {
            if let Some(mapped) = rewrite_single_pragma_stmt(t) {
                if !mapped.trim().is_empty() {
                    out.push(mapped);
                }
            }
            continue;
        }
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out.join("; ")
}

fn rewrite_single_pragma_stmt(stmt: &str) -> Option<String> {
    let original_stmt = stmt.trim().to_string();
    let mut body = stmt.trim();
    if body.len() < 6 || !body[..6].eq_ignore_ascii_case("pragma") {
        return None;
    }
    body = body[6..].trim();
    if body.is_empty() {
        return None;
    }

    // Drop optional schema qualifier (e.g. `main.journal_mode`)
    let body = if let Some((_, rest)) = body.rsplit_once('.') {
        rest.trim()
    } else {
        body
    };

    let (name_raw, is_set, value_opt) = if let Some((lhs, rhs)) = body.split_once('=') {
        (lhs.trim(), true, Some(rhs.trim()))
    } else if let Some(open) = body.find('(') {
        if body.ends_with(')') && open < body.len() - 1 {
            (
                body[..open].trim(),
                true,
                Some(body[open + 1..body.len() - 1].trim()),
            )
        } else {
            (body.trim(), false, None)
        }
    } else {
        (body.trim(), false, None)
    };

    if name_raw.is_empty() {
        return None;
    }
    let name = name_raw.trim_matches('`').trim_matches('"').to_ascii_lowercase();
    let normalized_value =
        value_opt.map(|v| v.trim_matches('\'').trim_matches('"').trim().to_ascii_lowercase());

    let (mapped, outcome) = match (name.as_str(), is_set) {
        // Assignment-like pragma calls: safe no-op.
        ("foreign_keys" | "case_sensitive_like", true) => {
            let _ = value_opt;
            ("SELECT 1".to_string(), "noop_set")
        }

        // busy_timeout maps best to PostgreSQL lock_timeout (session-level).
        ("busy_timeout", true) => {
            if let Some(raw) = value_opt {
                if let Ok(ms) = raw
                    .trim_matches('\'')
                    .trim_matches('"')
                    .trim()
                    .parse::<i64>()
                {
                    let clamped = ms.max(0);
                    (
                        format!("SELECT set_config('lock_timeout', '{}ms', true)", clamped),
                        "mapped_set",
                    )
                } else {
                    ("SELECT 1".to_string(), "noop_set")
                }
            } else {
                ("SELECT 1".to_string(), "noop_set")
            }
        }
        ("busy_timeout", false) => {
            (
                "SELECT current_setting('lock_timeout') AS busy_timeout".to_string(),
                "mapped_read",
            )
        }

        // foreign_keys ON/OFF has no safe direct PostgreSQL equivalent in this shim.
        ("foreign_keys", false) => ("SELECT 1 AS foreign_keys".to_string(), "mapped_read"),

        // journal_mode has no direct equivalent; keep it as session metadata.
        ("journal_mode", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "delete" | "truncate" | "persist" | "memory" | "wal" | "off" => v,
                    _ => String::new(),
                })
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.journal_mode', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("journal_mode", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.journal_mode', true), 'wal') AS journal_mode"
                .to_string(),
            "mapped_read",
        ),

        // synchronous maps to PostgreSQL synchronous_commit where safe.
        ("synchronous", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "off" => "off",
                    "1" | "normal" | "2" | "full" | "3" | "extra" | "on" => "on",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!("SELECT set_config('synchronous_commit', '{}', true)", mapped),
                    "mapped_set",
                )
            }
        }
        ("synchronous", false) => {
            (
                "SELECT current_setting('synchronous_commit') AS synchronous".to_string(),
                "mapped_read",
            )
        }

        // temp_store has no direct PG equivalent; preserve intent via custom session setting.
        ("temp_store", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "default" => "default",
                    "1" | "file" => "file",
                    "2" | "memory" => "memory",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.temp_store', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("temp_store", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.temp_store', true), 'memory') AS temp_store"
                .to_string(),
            "mapped_read",
        ),

        // cache_size has no exact PG equivalent; keep a custom session value for compatibility.
        ("cache_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.cache_size', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("cache_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.cache_size', true), '-2000') AS cache_size"
                .to_string(),
            "mapped_read",
        ),

        // page_size has no session-level equivalent; keep compatibility metadata.
        ("page_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!("SELECT set_config('plex.sqlite.page_size', '{}', true)", mapped),
                    "mapped_set",
                )
            }
        }
        ("page_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.page_size', true), '4096') AS page_size"
                .to_string(),
            "mapped_read",
        ),

        // auto_vacuum has no session-level equivalent; keep compatibility metadata.
        ("auto_vacuum", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "0" | "none" => "none",
                    "1" | "full" => "full",
                    "2" | "incremental" => "incremental",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.auto_vacuum', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("auto_vacuum", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.auto_vacuum', true), 'none') AS auto_vacuum"
                .to_string(),
            "mapped_read",
        ),

        // locking_mode has no direct PG equivalent; persist as session metadata.
        ("locking_mode", true) => {
            let mapped = normalized_value
                .map(|v| match v.as_str() {
                    "normal" => "normal",
                    "exclusive" => "exclusive",
                    _ => "",
                })
                .unwrap_or("");
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.locking_mode', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("locking_mode", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.locking_mode', true), 'normal') AS locking_mode"
                .to_string(),
            "mapped_read",
        ),

        // wal_autocheckpoint has no PG analog at session-level; persist as compatibility metadata.
        ("wal_autocheckpoint", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.wal_autocheckpoint', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("wal_autocheckpoint", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.wal_autocheckpoint', true), '1000') AS wal_autocheckpoint"
                .to_string(),
            "mapped_read",
        ),

        // mmap_size has no PG analog at session-level; persist as compatibility metadata.
        ("mmap_size", true) => {
            let mapped = normalized_value
                .filter(|v| v.parse::<i64>().is_ok())
                .unwrap_or_default();
            if mapped.is_empty() {
                ("SELECT 1".to_string(), "noop_set")
            } else {
                (
                    format!(
                        "SELECT set_config('plex.sqlite.mmap_size', '{}', true)",
                        mapped
                    ),
                    "mapped_set",
                )
            }
        }
        ("mmap_size", false) => (
            "SELECT COALESCE(current_setting('plex.sqlite.mmap_size', true), '0') AS mmap_size"
                .to_string(),
            "mapped_read",
        ),
        ("case_sensitive_like", false) => ("SELECT 0 AS case_sensitive_like".to_string(), "mapped_read"),

        // Unknown pragma: strip, unless strict mode is enabled.
        _ if pragma_strict_mode_enabled() => (STRICT_PRAGMA_ERROR_SQL.to_string(), "strict_fail"),
        _ => (String::new(), "stripped"),
    };
    pragma_trace_mapping(&original_stmt, &name, &mapped, outcome);
    Some(mapped)
}

fn pragma_trace_enabled() -> bool {
    std::env::var("PLEX_PG_TRACE_PRAGMA")
        .ok()
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on" | "debug")
        })
        .unwrap_or(false)
}

fn pragma_metrics_enabled() -> bool {
    std::env::var("PLEX_PG_TRACE_PRAGMA_METRICS")
        .ok()
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on" | "debug")
        })
        .unwrap_or(false)
}

fn pragma_strict_mode_enabled() -> bool {
    #[cfg(test)]
    {
        match TEST_STRICT_PRAGMA_OVERRIDE.load(Ordering::Relaxed) {
            0 => return false,
            1 => return true,
            _ => {}
        }
    }
    std::env::var("PLEX_PG_STRICT_PRAGMA")
        .ok()
        .map(|v| {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn pragma_trace_mapping(original_stmt: &str, pragma_name: &str, rewritten_sql: &str, outcome: &str) {
    match outcome {
        "mapped_set" => {
            PRAGMA_MAPPED_SET.fetch_add(1, Ordering::Relaxed);
        }
        "mapped_read" => {
            PRAGMA_MAPPED_READ.fetch_add(1, Ordering::Relaxed);
        }
        "noop_set" => {
            PRAGMA_NOOP_SET.fetch_add(1, Ordering::Relaxed);
        }
        "strict_fail" => {
            PRAGMA_STRICT_FAIL.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            PRAGMA_STRIPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    let total = PRAGMA_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;

    if pragma_trace_enabled() {
        if rewritten_sql.trim().is_empty() {
            eprintln!(
                "[PRAGMA_MAP] {} [{}:{}] -> <stripped>",
                original_stmt, pragma_name, outcome
            );
        } else {
            eprintln!(
                "[PRAGMA_MAP] {} [{}:{}] -> {}",
                original_stmt, pragma_name, outcome, rewritten_sql
            );
        }
    }

    if pragma_metrics_enabled() && total.is_multiple_of(50) {
        eprintln!(
            "[PRAGMA_MAP] stats total={} mapped_set={} mapped_read={} noop_set={} stripped={} strict_fail={}",
            total,
            PRAGMA_MAPPED_SET.load(Ordering::Relaxed),
            PRAGMA_MAPPED_READ.load(Ordering::Relaxed),
            PRAGMA_NOOP_SET.load(Ordering::Relaxed),
            PRAGMA_STRIPPED.load(Ordering::Relaxed),
            PRAGMA_STRICT_FAIL.load(Ordering::Relaxed),
        );
    }
}

/// Rewrite SQLite virtual table DDL to PostgreSQL-compatible CREATE TABLE.
///
/// - `CREATE VIRTUAL TABLE x USING fts5(a,b)` ->
///   `CREATE TABLE x (id BIGSERIAL PRIMARY KEY, a TEXT, b TEXT, _fts TSVECTOR)`
///
/// - `CREATE VIRTUAL TABLE x USING rtree(id,minX,maxX,...)` ->
///   `CREATE TABLE x (id DOUBLE PRECISION, minX DOUBLE PRECISION, ...)`
fn rewrite_virtual_tables(sql: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for stmt in split_sql_statements(sql) {
        let t = stmt.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rewritten) = rewrite_single_virtual_table_stmt(t) {
            out.push(rewritten);
        } else {
            out.push(t.to_string());
        }
    }
    out.join("; ")
}

fn rewrite_single_virtual_table_stmt(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    let prefix = "create virtual table ";
    if !lower.starts_with(prefix) {
        return None;
    }

    let using_fts = " using fts5(";
    let using_rtree = " using rtree(";
    let (using_idx, mode, using_len) = if let Some(idx) = lower.find(using_fts) {
        (idx, "fts5", using_fts.len())
    } else if let Some(idx) = lower.find(using_rtree) {
        (idx, "rtree", using_rtree.len())
    } else {
        return None;
    };

    let table_name = stmt[prefix.len()..using_idx].trim();
    if table_name.is_empty() {
        return None;
    }

    let cols_start = using_idx + using_len;
    let cols_end = stmt.rfind(')')?;
    if cols_end < cols_start {
        return None;
    }
    let cols_raw = &stmt[cols_start..cols_end];
    let cols = split_csv_top_level(cols_raw);

    let mut defs: Vec<String> = Vec::new();
    if mode == "fts5" {
        defs.push("id BIGSERIAL PRIMARY KEY".to_string());
        for c in cols {
            let col = extract_ident_token(&c);
            if col.is_empty() || col.to_ascii_lowercase().starts_with("tokenize") {
                continue;
            }
            defs.push(format!("{} TEXT", col));
        }
        defs.push("_fts TSVECTOR".to_string());
    } else {
        for c in cols {
            let col = extract_ident_token(&c);
            if col.is_empty() {
                continue;
            }
            defs.push(format!("{} DOUBLE PRECISION", col));
        }
    }

    if defs.is_empty() {
        return None;
    }
    Some(format!("CREATE TABLE {} ({})", table_name, defs.join(", ")))
}

fn split_sql_statements(sql: &str) -> Vec<String> {
    // SQLite trigger bodies contain internal semicolons inside BEGIN...END blocks.
    // Treat CREATE TRIGGER statements as a single unit to avoid splitting mid-body.
    if sql.to_ascii_lowercase().contains("create trigger") {
        let t = sql.trim();
        if t.is_empty() {
            return Vec::new();
        }
        return vec![t.to_string()];
    }

    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;

    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\'' if !in_double => {
                cur.push(ch);
                if in_single && i + 1 < chars.len() && chars[i + 1] == '\'' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                cur.push(ch);
                if in_double && i + 1 < chars.len() && chars[i + 1] == '"' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_double = !in_double;
                }
            }
            ';' if !in_single && !in_double => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn split_csv_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\'' if !in_double => {
                cur.push(ch);
                if in_single && i + 1 < chars.len() && chars[i + 1] == '\'' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                cur.push(ch);
                if in_double && i + 1 < chars.len() && chars[i + 1] == '"' {
                    cur.push(chars[i + 1]);
                    i += 1;
                } else {
                    in_double = !in_double;
                }
            }
            '(' if !in_single && !in_double => {
                depth += 1;
                cur.push(ch);
            }
            ')' if !in_single && !in_double => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            ',' if !in_single && !in_double && depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn extract_ident_token(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    let tok = t.split_whitespace().next().unwrap_or("");
    tok.trim_matches(',').to_string()
}

// Plex self-join compatibility:
//
// Plex emits two related query shapes that PostgreSQL rejects:
//
// Shape A – metadata_item_settings as root, metadata_items joined twice (one aliased, one NOT):
//   SELECT ... FROM metadata_item_settings
//   JOIN metadata_items AS parents  ON parents.id = metadata_items.parent_id
//   JOIN metadata_items             ON metadata_items.id = metadata_item_settings.metadata_item_id
//   WHERE metadata_items.col = ...
//
//   Problem: `metadata_items` appears as alias-target (`AS parents`) AND as an
//   unaliased join + bare column qualifier. PostgreSQL raises:
//     "invalid reference to FROM-clause entry for table metadata_items"
//
//   Fix: add `AS mi` to the unaliased join and replace every
//        `metadata_items.<col>` reference with `mi.<col>`.
//
// Shape B – metadata_items as root, self-joined via parents / grandparents:
//   SELECT metadata_items.id FROM metadata_items
//   JOIN metadata_items AS parents      ON parents.id = metadata_items.parent_id
//   JOIN metadata_items AS grandparents ON grandparents.id = parents.parent_id
//   WHERE metadata_items.library_section_id IN (...)
//   ORDER BY grandparents.title_sort ...
//
//   In this shape the base table IS `metadata_items` (unaliased), so bare
//   `metadata_items.col` refs are valid in PostgreSQL.  Nothing to rewrite.
//   (The COLLATE icu_root / parse-error is handled by rewrite_sqlite_collations
//   which runs before this function.)
fn rewrite_metadata_items_self_join(sql: &str) -> String {
    let lower = sql.to_lowercase();

    // ── Shape A detection ──────────────────────────────────────────────────────
    // Plex emits queries like:
    //   SELECT metadata_items.id FROM metadata_item_settings
    //   JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id
    //   JOIN metadata_items ON metadata_items.id = metadata_item_settings.metadata_item_id
    //   WHERE metadata_items.col = ...
    //
    // PostgreSQL rejects this because `metadata_items` is both an alias target
    // and used as a bare qualifier.
    //
    // Fix strategy: reorder the joins so the unaliased metadata_items join comes
    // FIRST (renamed to AS mi), then the aliased joins (parents, etc.) can refer
    // to mi.col in their ON clauses. Then replace metadata_items.col → mi.col
    // throughout SELECT/WHERE/HAVING/ORDER BY.
    if lower.contains("from metadata_item_settings") && lower.contains("join metadata_items") {
        let total_joins = count_word_occurrences(&lower, "join metadata_items");
        let aliased_joins = count_word_occurrences(&lower, "join metadata_items as ");
        let unaliased_joins = total_joins.saturating_sub(aliased_joins);

        if unaliased_joins > 0 && aliased_joins > 0 {
            return reorder_and_alias_self_join(sql);
        }
    }

    sql.to_string()
}

/// Rewrite Shape A self-join query by:
/// 1. Extracting the unaliased "JOIN metadata_items ON ..." fragment
/// 2. Placing it first in the join list as "JOIN metadata_items AS mi ON ..."
///    (rewriting its ON clause: metadata_items.col → mi.col)
/// 3. Rewriting the ON clauses of subsequent aliased joins: metadata_items.col → mi.col
/// 4. Replacing all remaining metadata_items.col references in SELECT/WHERE/etc.
fn reorder_and_alias_self_join(sql: &str) -> String {
    let lower = sql.to_lowercase();

    // Split into: prefix (everything up to the first JOIN), list of join fragments,
    // and suffix (WHERE + rest after all JOINs).
    //
    // We tokenise by " join " (case-insensitive).
    // The prefix is the part before the first join.
    let join_needle = " join ";
    let first_join = lower.find(join_needle);
    if first_join.is_none() {
        return sql.to_string();
    }
    let first_join_pos = first_join.unwrap();

    // prefix: "SELECT ... FROM metadata_item_settings"
    let prefix = &sql[..first_join_pos];

    // Split the rest into join fragments. Each fragment starts after " join " and
    // ends just before the next " join " (or at WHERE/end).
    let rest = &sql[first_join_pos + join_needle.len()..];
    let rest_lower = &lower[first_join_pos + join_needle.len()..];

    // Find WHERE, HAVING, ORDER BY, LIMIT, GROUP BY at top level (not inside parens)
    let suffix_start = find_top_level_clause(rest_lower);
    let joins_str = &rest[..suffix_start];
    let suffix = &rest[suffix_start..];

    // Split joins_str into individual join fragments by " join "
    let mut join_fragments: Vec<String> = Vec::new();
    let joins_lower = joins_str.to_lowercase();
    let mut pos = 0usize;
    loop {
        match joins_lower[pos..].find(join_needle) {
            None => {
                join_fragments.push(joins_str[pos..].to_string());
                break;
            }
            Some(rel) => {
                join_fragments.push(joins_str[pos..pos + rel].to_string());
                pos += rel + join_needle.len();
            }
        }
    }

    // Identify which fragment(s) are unaliased metadata_items joins.
    // An unaliased fragment starts with "metadata_items " or "metadata_items\n"
    // but NOT "metadata_items as".
    let mut unaliased_idxs: Vec<usize> = Vec::new();
    let mut aliased_idxs: Vec<usize> = Vec::new();
    for (idx, frag) in join_fragments.iter().enumerate() {
        let fl = frag.to_lowercase();
        let fl = fl.trim_start();
        if let Some(after) = fl.strip_prefix("metadata_items") {
            let after = after.trim_start();
            if after.starts_with("as ") || after.starts_with("as\t") {
                aliased_idxs.push(idx);
            } else {
                unaliased_idxs.push(idx);
            }
        } else {
            aliased_idxs.push(idx);
        }
    }

    if unaliased_idxs.is_empty() {
        return sql.to_string();
    }

    // Build the new join list: unaliased first (as mi), then aliased
    let mut new_joins: Vec<String> = Vec::new();

    for &idx in &unaliased_idxs {
        let frag = &join_fragments[idx];
        let fl = frag.to_lowercase();
        let fl_trim = fl.trim_start();
        // Strip leading "metadata_items" and get the rest (ON clause etc.)
        let rest_of_frag =
            &frag[frag.to_lowercase().find("metadata_items").unwrap() + "metadata_items".len()..];
        // Rewrite metadata_items.col → mi.col in the ON clause of this fragment
        let on_fixed = rest_of_frag.replace("metadata_items.", "mi.");
        let _ = fl_trim; // suppress warning
        new_joins.push(format!("metadata_items AS mi{}", on_fixed));
    }

    for &idx in &aliased_idxs {
        let frag = &join_fragments[idx];
        // In aliased join ON clauses, replace metadata_items.col → mi.col
        let fixed = frag.replace("metadata_items.", "mi.");
        new_joins.push(fixed);
    }

    // Reconstruct: prefix + JOIN fragment1 JOIN fragment2 ... + suffix
    let joins_part = new_joins.join(" JOIN ");
    let reconstructed = format!("{} JOIN {} {}", prefix, joins_part, suffix);

    // Finally replace remaining metadata_items.col in SELECT/WHERE/etc.
    replace_metadata_items_refs(&reconstructed, "mi")
}

/// Find the byte offset in `s` (lowercase) where a top-level SQL clause starts
/// (WHERE, HAVING, ORDER BY, GROUP BY, LIMIT, UNION). Returns s.len() if none found.
fn find_top_level_clause(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'\'' => {
                // skip string literal
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ if depth == 0 => {
                for kw in &[
                    "where ",
                    "having ",
                    "order by ",
                    "group by ",
                    "limit ",
                    "union ",
                ] {
                    if s[i..].starts_with(kw) {
                        return i;
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    s.len()
}

/// Count non-overlapping occurrences of `needle` in `haystack` (both lowercase).
fn count_word_occurrences(haystack: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        count += 1;
        start += pos + needle.len();
    }
    count
}

/// Replace every occurrence of `metadata_items.<identifier>` with
/// `<new_alias>.<identifier>` throughout the SQL, being careful not to
/// touch occurrences inside quoted string literals.
fn replace_metadata_items_refs(sql: &str, new_alias: &str) -> String {
    let needle = "metadata_items.";
    let needle_lower = needle.to_lowercase();
    let sql_lower = sql.to_lowercase();

    let mut result = String::with_capacity(sql.len());
    let mut i = 0usize;
    let bytes = sql.as_bytes();

    while i < bytes.len() {
        // Skip single-quoted string literals verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i];
                result.push(ch as char);
                i += 1;
                if ch == b'\'' {
                    // escaped '' or end of string
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            continue;
        }

        // Check for "metadata_items." at current position
        if sql_lower[i..].starts_with(&needle_lower) {
            // Emit the alias instead
            result.push_str(new_alias);
            result.push('.');
            i += needle.len();
            continue;
        }

        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

#[cfg(test)]
mod self_join_tests {
    use super::preprocess;

    // ── Shape A tests ──────────────────────────────────────────────────────────

    /// Original failing pattern: FROM metadata_item_settings + aliased parents join
    /// + UNALIASED metadata_items join.  The unaliased join must get AS mi and all
    /// metadata_items.<col> refs must become mi.<col>.
    #[test]
    fn preprocess_rewrites_metadata_items_self_join_alias_refs() {
        // Shape A: unaliased second join that needs AS mi
        let input = concat!(
            "SELECT metadata_items.id, metadata_items.title ",
            "FROM metadata_item_settings ",
            "JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id ",
            "JOIN metadata_items ON metadata_items.id = metadata_item_settings.metadata_item_id ",
            "WHERE metadata_items.library_section_id = 1"
        );
        let out = preprocess(input);
        // The unaliased JOIN should have AS mi now
        assert!(
            out.to_lowercase().contains("join metadata_items as mi"),
            "unaliased join should get AS mi, out={}",
            out
        );
        // All metadata_items.<col> should be mi.<col>
        assert!(
            !out.to_lowercase().contains("metadata_items."),
            "no bare metadata_items. refs should remain, out={}",
            out
        );
        // The aliased parents join must stay
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as parents"),
            "parents alias must be preserved, out={}",
            out
        );
        // mi. refs are present
        assert!(
            out.to_lowercase().contains("mi.id"),
            "mi.id should appear, out={}",
            out
        );
    }

    /// Legacy test: all joins already aliased — nothing should change (no unaliased join).
    #[test]
    fn preprocess_no_rewrite_when_all_joins_aliased() {
        let input = concat!(
            "select metadata_items.id from metadata_item_settings ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id"
        );
        let out = preprocess(input);
        // When all joins are aliased there is no unaliased join to fix, so the
        // function should return the input unchanged (aside from other preprocess steps).
        // Both aliases must still be present.
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as parents"),
            "parents alias must stay, out={}",
            out
        );
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as grandparents"),
            "grandparents alias must stay, out={}",
            out
        );
        // No spurious AS mi should have been injected
        assert!(
            !out.to_lowercase().contains("as mi"),
            "no AS mi should appear when all joins aliased, out={}",
            out
        );
    }

    /// Shape B (from-metadata_items root): no rewrite should happen — base table
    /// IS metadata_items so bare refs are valid in PostgreSQL.
    #[test]
    fn preprocess_no_rewrite_for_shape_b_metadata_items_root() {
        let input = concat!(
            "select metadata_items.id from metadata_items ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id ",
            "where metadata_items.library_section_id in (2)"
        );
        let out = preprocess(input);
        // FROM is metadata_items, NOT metadata_item_settings, so no rewrite fires.
        assert!(
            !out.to_lowercase().contains("as mi"),
            "no AS mi for shape-B query, out={}",
            out
        );
    }
}

/// Fix `?identifier` placeholders that sqlparser can't handle.
/// SQLite allows `?left` (positional placeholder followed by identifier without space),
/// but sqlparser chokes on it because `left` is a keyword.
/// We strip the trailing identifier part: `?left` → `?`.
fn fix_placeholder_spacing(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;
    let mut in_string = false;
    let mut string_char: u8 = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            result.push(b as char);
            if b == string_char {
                if i + 1 < bytes.len() && bytes[i + 1] == string_char {
                    result.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                in_string = false;
            }
            i += 1;
            continue;
        }

        if b == b'\'' || b == b'"' {
            in_string = true;
            string_char = b;
            result.push(b as char);
            i += 1;
            continue;
        }

        if b == b'?' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            // `?keyword` — SQLite allows a placeholder immediately followed by a SQL keyword
            // with no space (e.g. `=?group by`). sqlparser fails to parse this.
            // Read the word that follows the `?` and check if it's a SQL keyword.
            // If it is, insert a space so the keyword is recognised: `?group` → `? group`.
            // If it's just an identifier suffix (e.g. `?left` used as a named param),
            // strip the identifier to keep it as a single `?`.
            let word_start = i + 1;
            let mut word_end = word_start;
            while word_end < bytes.len()
                && (bytes[word_end].is_ascii_alphanumeric() || bytes[word_end] == b'_')
            {
                word_end += 1;
            }
            let word = std::str::from_utf8(&bytes[word_start..word_end])
                .unwrap_or("")
                .to_uppercase();
            // SQL clause/operator keywords that can legally follow a placeholder value
            // but are never valid SQLite named-parameter suffixes.
            // Excludes things like LEFT/RIGHT/JOIN/FROM which Plex uses as param names.
            const SQL_KEYWORDS: &[&str] = &[
                "GROUP",
                "ORDER",
                "HAVING",
                "LIMIT",
                "UNION",
                "EXCEPT",
                "INTERSECT",
                "WHERE",
                "AND",
                "OR",
                "NOT",
                "BETWEEN",
                "GLOB",
                "THEN",
                "ELSE",
                "END",
                "WHEN",
                "CASE",
            ];
            result.push('?');
            if SQL_KEYWORDS.contains(&word.as_str()) {
                // Insert space so the keyword is separately tokenised
                result.push(' ');
                i += 1; // just skip the `?`, keep the keyword as-is
            } else {
                // Non-keyword suffix: strip it (was a SQLite named param like ?left)
                i = word_end;
            }
            continue;
        }

        result.push(b as char);
        i += 1;
    }
    result
}

/// Replace `GLOB '<pattern>'` with `ILIKE '<pg_pattern>'`.
/// Uses a simple state machine to avoid touching string literals.
fn rewrite_glob(sql: &str) -> String {
    // We do a case-insensitive scan for the word GLOB followed by a quoted string.
    // Strategy: tokenise by single-quoted strings to avoid false positives inside literals.
    let upper = sql.to_uppercase();
    // Fast path – nothing to do
    if !upper.contains("GLOB") {
        return sql.to_string();
    }

    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;

    while i < bytes.len() {
        // Inside a single-quoted string → copy verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    // Escaped quote ''
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                result.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // Check for GLOB keyword (case-insensitive, word boundary)
        let rest = &sql[i..];
        let rest_upper = &upper[i..];
        if rest_upper.starts_with("GLOB") {
            // Must be followed by whitespace / quote (word boundary)
            let after = i + 4;
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                // Skip "GLOB"
                i += 4;
                // Skip whitespace
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                // Now we should be at the opening quote of the pattern
                if i < bytes.len() && bytes[i] == b'\'' {
                    i += 1; // skip opening quote
                    let mut pattern = String::new();
                    while i < bytes.len() {
                        if bytes[i] == b'\'' {
                            i += 1;
                            if i < bytes.len() && bytes[i] == b'\'' {
                                pattern.push('\'');
                                i += 1;
                            } else {
                                break;
                            }
                        } else {
                            pattern.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    // Translate wildcards: * → %, ? → _
                    let pg_pattern: String = pattern
                        .chars()
                        .map(|c| match c {
                            '*' => '%',
                            '?' => '_',
                            other => other,
                        })
                        .collect();
                    result.push_str(&format!("ILIKE '{}'", pg_pattern));
                } else {
                    // No quote found – emit ILIKE as-is and continue
                    result.push_str("ILIKE ");
                }
                continue;
            }
        }

        result.push(rest.chars().next().unwrap());
        i += rest.chars().next().map_or(1, |c| c.len_utf8());
    }

    result
}

/// Remove SQLite index hints:
/// - `INDEXED BY <identifier>`
/// - `NOT INDEXED`
fn rewrite_indexed_by(sql: &str) -> String {
    let upper = sql.to_uppercase();
    if !upper.contains("INDEXED") {
        return sql.to_string();
    }

    // Find all occurrences of INDEXED BY <ident> and remove them.
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;
    let bytes = sql.as_bytes();

    while i < bytes.len() {
        // Skip single-quoted strings verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                result.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        let rest_upper = &upper[i..];
        if rest_upper.starts_with("NOT INDEXED") {
            let after = i + "NOT INDEXED".len();
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                i = after;
                continue;
            }
        }
        if rest_upper.starts_with("INDEXED") {
            let after = i + 7;
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                // Skip "INDEXED"
                let mut j = i + 7;
                // Skip whitespace
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Check for BY
                if sql[j..].to_uppercase().starts_with("BY") {
                    j += 2;
                    // Skip whitespace
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    // Skip identifier
                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
                    {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }

        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

/// Remove `COLLATE <name>` clauses for SQLite-specific collations that either
/// sqlparser (SQLiteDialect) cannot parse, or that have no PostgreSQL equivalent.
///
/// ICU extension names like `icu_root` and `icu_und`, as well as `UNICODE` /
/// `UNICODE_CI`, are not in sqlparser's keyword list and cause a hard parse error:
///   "Expected: end of statement, found: collate"
///
/// `RTRIM` and `BINARY` parse fine but PostgreSQL has no matching collation, so
/// they are dropped here as well.
///
/// `NOCASE` is intentionally left intact; the AST-level transform in query.rs
/// converts it to LOWER(…) which is the correct PostgreSQL semantic.
fn rewrite_sqlite_collations(sql: &str) -> String {
    let upper = sql.to_uppercase();
    // Fast path — nothing to do
    if !upper.contains("COLLATE") {
        return sql.to_string();
    }

    let bytes = sql.as_bytes();
    let mut result = String::with_capacity(sql.len());
    let mut i = 0;

    while i < bytes.len() {
        // Skip single-quoted string literals verbatim
        if bytes[i] == b'\'' {
            result.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    result.push('\'');
                    i += 1;
                    // Escaped quote: ''
                    if i < bytes.len() && bytes[i] == b'\'' {
                        result.push('\'');
                        i += 1;
                    }
                    break;
                }
                result.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // Skip double-quoted identifiers verbatim
        if bytes[i] == b'"' {
            result.push('"');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    result.push('"');
                    i += 1;
                    // Escaped double-quote: ""
                    if i < bytes.len() && bytes[i] == b'"' {
                        result.push('"');
                        i += 1;
                    }
                    break;
                }
                result.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // Skip backtick-quoted identifiers verbatim
        if bytes[i] == b'`' {
            result.push('`');
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                result.push(ch);
                i += 1;
                if ch == '`' {
                    break;
                }
            }
            continue;
        }

        // Check for COLLATE keyword (case-insensitive, word boundary)
        let rest_upper = &upper[i..];
        if rest_upper.starts_with("COLLATE") {
            let after = i + 7;
            let boundary = after >= sql.len()
                || !sql[after..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary {
                // Skip optional whitespace after COLLATE
                let mut j = after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Read the collation name (letters, digits, underscores, hyphens)
                let name_start = j;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
                {
                    j += 1;
                }
                let collation_name = sql[name_start..j].to_uppercase();

                // Strip SQLite-only collations that have no direct PostgreSQL equivalent.
                //
                // ICU names (icu_root, icu_und, …) and unicode/unicode_ci are not
                // recognised by sqlparser/SQLiteDialect and cause a hard parse error:
                //   "Expected: end of statement, found: collate"
                //
                // RTRIM and BINARY parse fine, but PostgreSQL has no matching collation
                // so they are stripped here too.  NOCASE is intentionally left for the
                // AST-level handler in query.rs which converts it to LOWER(…).
                let should_strip = collation_name.starts_with("ICU")
                    || collation_name == "UNICODE"
                    || collation_name == "UNICODE_CI"
                    || collation_name == "RTRIM"
                    || collation_name == "BINARY";

                if should_strip {
                    // Skip the entire `COLLATE <name>` token.
                    // Also consume one leading space before COLLATE (if any) to avoid
                    // leaving a double-space artefact.
                    if result.ends_with(' ') {
                        result.pop();
                    }
                    i = j;
                    continue;
                }
            }
        }

        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

// ─── AST-level transformations ────────────────────────────────────────────────

fn transform_query(q: &mut Query) {
    if let Some(with) = &mut q.with {
        for cte in &mut with.cte_tables {
            transform_query(&mut cte.query);
        }
    }

    // Detect GROUP BY NULL before transforming (need Query-level access for ORDER BY)
    let had_group_by_null = if let SetExpr::Select(sel) = q.body.as_ref() {
        if let GroupByExpr::Expressions(exprs, _) = &sel.group_by {
            !exprs.is_empty() && exprs.iter().all(is_null_expr)
        } else {
            false
        }
    } else {
        false
    };

    transform_set_expr(&mut q.body);

    // GROUP BY NULL was removed: add ORDER BY 1 NULLS FIRST if no ORDER BY exists
    if had_group_by_null && q.order_by.is_none() {
        q.order_by = Some(OrderBy {
            kind: OrderByKind::Expressions(vec![OrderByExpr {
                expr: Expr::Value(ValueWithSpan {
                    value: Value::Number("1".to_string(), false),
                    span: Span::empty(),
                }),
                options: OrderByOptions {
                    asc: None,
                    nulls_first: Some(true),
                },
                with_fill: None,
            }]),
            interpolate: None,
        });
    }
}

fn transform_set_expr(se: &mut SetExpr) {
    match se {
        SetExpr::Select(s) => transform_select(s),
        SetExpr::Query(q) => transform_query(q),
        SetExpr::SetOperation { left, right, .. } => {
            transform_set_expr(left);
            transform_set_expr(right);
        }
        _ => {}
    }
}

fn transform_select(sel: &mut Select) {
    // Rewrite FROM tables (sqlite_master → information_schema subquery)
    for twj in &mut sel.from {
        transform_table_with_joins(twj);
    }

    // WHERE
    if let Some(w) = &mut sel.selection {
        transform_expr(w);
    }

    // GROUP BY NULL → remove
    if let GroupByExpr::Expressions(exprs, _) = &mut sel.group_by {
        exprs.retain(|e| !is_null_expr(e));
        // If all expressions were NULL, replace with empty group-by
    }

    // Projection subqueries
    for item in &mut sel.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                transform_expr(e);
            }
            _ => {}
        }
    }

    // HAVING
    if let Some(h) = &mut sel.having {
        transform_expr(h);
    }
}

fn is_null_expr(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Value(ValueWithSpan {
            value: Value::Null,
            ..
        })
    )
}

fn transform_table_with_joins(twj: &mut TableWithJoins) {
    transform_table_factor(&mut twj.relation);
    for join in &mut twj.joins {
        transform_table_factor(&mut join.relation);
    }
}

fn transform_table_factor(tf: &mut TableFactor) {
    match tf {
        TableFactor::Table { name, .. } => {
            // Check the last part of the name (handles "main".sqlite_master etc.)
            let bare_name = table_name_last_part(name);
            if bare_name == "sqlite_master" || bare_name == "sqlite_schema" {
                *tf = make_sqlite_master_subquery();
            }
        }
        TableFactor::Derived { subquery, .. } => {
            transform_query(subquery);
        }
        _ => {}
    }
}

/// Get the last (bare) part of a table name, e.g. "main".sqlite_master → sqlite_master
fn table_name_last_part(name: &ObjectName) -> String {
    name.0
        .last()
        .and_then(|p| match p {
            ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Build the subquery that replaces `sqlite_master`:
///
/// ```sql
/// (SELECT tablename AS name, 'table' AS type
///  FROM information_schema.tables WHERE table_schema = 'public'
///  UNION
///  SELECT indexname AS name, 'index' AS type
///  FROM pg_indexes WHERE schemaname = 'public') AS sqlite_master
/// ```
fn make_sqlite_master_subquery() -> TableFactor {
    let make_str = |s: &str| -> Expr {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s.to_string()),
            span: Span::empty(),
        })
    };
    let make_ident = |s: &str| -> Expr { Expr::Identifier(Ident::new(s)) };
    let make_alias_item = |expr: Expr, alias: &str| -> SelectItem {
        SelectItem::ExprWithAlias {
            expr,
            alias: Ident::new(alias),
        }
    };

    // SELECT tablename AS name, 'table' AS type
    // FROM information_schema.tables
    // WHERE table_schema = 'public'
    let tables_select = Box::new(SetExpr::Select(Box::new(Select {
        select_token: AttachedToken::empty(),
        optimizer_hint: None,
        distinct: None,
        select_modifiers: None,
        top: None,
        top_before_distinct: false,
        projection: vec![
            make_alias_item(make_ident("tablename"), "name"),
            make_alias_item(make_str("table"), "type"),
        ],
        exclude: None,
        into: None,
        from: vec![TableWithJoins {
            relation: TableFactor::Table {
                name: ObjectName(vec![
                    ObjectNamePart::Identifier(Ident::new("information_schema")),
                    ObjectNamePart::Identifier(Ident::new("tables")),
                ]),
                alias: None,
                args: None,
                with_hints: vec![],
                version: None,
                with_ordinality: false,
                partitions: vec![],
                json_path: None,
                sample: None,
                index_hints: vec![],
            },
            joins: vec![],
        }],
        lateral_views: vec![],
        prewhere: None,
        selection: Some(Expr::BinaryOp {
            left: Box::new(make_ident("table_schema")),
            op: BinaryOperator::Eq,
            right: Box::new(make_str("public")),
        }),
        connect_by: vec![],
        group_by: GroupByExpr::Expressions(vec![], vec![]),
        cluster_by: vec![],
        distribute_by: vec![],
        sort_by: vec![],
        having: None,
        named_window: vec![],
        qualify: None,
        window_before_qualify: false,
        value_table_mode: None,
        flavor: SelectFlavor::Standard,
    })));

    // SELECT indexname AS name, 'index' AS type
    // FROM pg_indexes
    // WHERE schemaname = 'public'
    let indexes_select = Box::new(SetExpr::Select(Box::new(Select {
        select_token: AttachedToken::empty(),
        optimizer_hint: None,
        distinct: None,
        select_modifiers: None,
        top: None,
        top_before_distinct: false,
        projection: vec![
            make_alias_item(make_ident("indexname"), "name"),
            make_alias_item(make_str("index"), "type"),
        ],
        exclude: None,
        into: None,
        from: vec![TableWithJoins {
            relation: TableFactor::Table {
                name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("pg_indexes"))]),
                alias: None,
                args: None,
                with_hints: vec![],
                version: None,
                with_ordinality: false,
                partitions: vec![],
                json_path: None,
                sample: None,
                index_hints: vec![],
            },
            joins: vec![],
        }],
        lateral_views: vec![],
        prewhere: None,
        selection: Some(Expr::BinaryOp {
            left: Box::new(make_ident("schemaname")),
            op: BinaryOperator::Eq,
            right: Box::new(make_str("public")),
        }),
        connect_by: vec![],
        group_by: GroupByExpr::Expressions(vec![], vec![]),
        cluster_by: vec![],
        distribute_by: vec![],
        sort_by: vec![],
        having: None,
        named_window: vec![],
        qualify: None,
        window_before_qualify: false,
        value_table_mode: None,
        flavor: SelectFlavor::Standard,
    })));

    let union_query = Box::new(Query {
        with: None,
        body: Box::new(SetExpr::SetOperation {
            op: SetOperator::Union,
            set_quantifier: SetQuantifier::None,
            left: tables_select,
            right: indexes_select,
        }),
        order_by: None,
        limit_clause: None,
        fetch: None,
        locks: vec![],
        for_clause: None,
        settings: None,
        format_clause: None,
        pipe_operators: vec![],
    });

    TableFactor::Derived {
        lateral: false,
        subquery: union_query,
        alias: Some(TableAlias {
            explicit: true,
            name: Ident::new("pg_schema_objects"),
            columns: vec![],
        }),
        sample: None,
    }
}

/// Recursively transform an expression in place, rewriting SQLite keyword
/// constructs to their PostgreSQL equivalents.
fn transform_expr(expr: &mut Expr) {
    match expr {
        // IN () → IN (SELECT -1 WHERE FALSE)
        Expr::InList { ref list, .. } if list.is_empty() => {
            // Build: SELECT -1 WHERE FALSE
            let false_select = Box::new(Query {
                with: None,
                body: Box::new(SetExpr::Select(Box::new(Select {
                    select_token: AttachedToken::empty(),
                    optimizer_hint: None,
                    distinct: None,
                    select_modifiers: None,
                    top: None,
                    top_before_distinct: false,
                    projection: vec![SelectItem::UnnamedExpr(Expr::UnaryOp {
                        op: UnaryOperator::Minus,
                        expr: Box::new(Expr::Value(ValueWithSpan {
                            value: Value::Number("1".to_string(), false),
                            span: Span::empty(),
                        })),
                    })],
                    exclude: None,
                    into: None,
                    from: vec![],
                    lateral_views: vec![],
                    prewhere: None,
                    selection: Some(Expr::Value(ValueWithSpan {
                        value: Value::Boolean(false),
                        span: Span::empty(),
                    })),
                    connect_by: vec![],
                    group_by: GroupByExpr::Expressions(vec![], vec![]),
                    cluster_by: vec![],
                    distribute_by: vec![],
                    sort_by: vec![],
                    having: None,
                    named_window: vec![],
                    qualify: None,
                    window_before_qualify: false,
                    value_table_mode: None,
                    flavor: SelectFlavor::Standard,
                }))),
                order_by: None,
                limit_clause: None,
                fetch: None,
                locks: vec![],
                for_clause: None,
                settings: None,
                format_clause: None,
                pipe_operators: vec![],
            });
            // Extract the inner expr, then replace the whole node
            let inner = match std::mem::replace(
                expr,
                Expr::Value(ValueWithSpan {
                    value: Value::Null,
                    span: Span::empty(),
                }),
            ) {
                Expr::InList {
                    expr: inner,
                    negated,
                    ..
                } => (inner, negated),
                _ => unreachable!(),
            };
            *expr = Expr::InSubquery {
                expr: inner.0,
                subquery: false_select,
                negated: inner.1,
            };
        }
        // Recurse
        Expr::BinaryOp { left, right, .. } => {
            transform_expr(left);
            transform_expr(right);
        }
        Expr::UnaryOp { expr: inner, .. } => transform_expr(inner),
        Expr::Nested(inner) => transform_expr(inner),
        Expr::InList {
            expr: inner, list, ..
        } => {
            transform_expr(inner);
            for e in list {
                transform_expr(e);
            }
        }
        Expr::InSubquery { expr: inner, .. } => {
            transform_expr(inner);
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            transform_expr(inner);
            transform_expr(low);
            transform_expr(high);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                transform_expr(op);
            }
            for cw in conditions {
                transform_expr(&mut cw.condition);
                transform_expr(&mut cw.result);
            }
            if let Some(er) = else_result {
                transform_expr(er);
            }
        }
        _ => {}
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{preprocess, Ordering, TEST_STRICT_PRAGMA_OVERRIDE};
    use crate::translate;
    use crate::test_utils::env_lock;

    #[test]
    fn keyword_begin_immediate() {
        let r = translate("BEGIN IMMEDIATE").unwrap();
        assert!(!r.sql.to_uppercase().contains("IMMEDIATE"));
        assert!(!r.sql.to_uppercase().contains("DEFERRED"));
        assert!(!r.sql.to_uppercase().contains("EXCLUSIVE"));
    }

    #[test]
    fn keyword_begin_deferred() {
        let r = translate("BEGIN DEFERRED").unwrap();
        assert!(!r.sql.to_uppercase().contains("DEFERRED"));
    }

    #[test]
    fn keyword_begin_exclusive() {
        let r = translate("BEGIN EXCLUSIVE").unwrap();
        assert!(!r.sql.to_uppercase().contains("EXCLUSIVE"));
    }

    #[test]
    fn keyword_end_to_commit() {
        let r = translate("END").unwrap();
        assert!(r.sql.to_uppercase().contains("COMMIT"));
    }

    #[test]
    fn keyword_release_without_savepoint_keyword() {
        let r = translate("RELEASE sp1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("RELEASE SAVEPOINT SP1"), "{}", r.sql);
    }

    #[test]
    fn keyword_rollback_transaction_to() {
        let r = translate("ROLLBACK TRANSACTION TO sp1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("ROLLBACK TO SAVEPOINT SP1"), "{}", r.sql);
    }

    #[test]
    fn keyword_glob_wildcard() {
        let r = translate("SELECT * FROM t WHERE name GLOB '*test*'").unwrap();
        assert!(r.sql.to_uppercase().contains("ILIKE") || r.sql.to_uppercase().contains("LIKE"));
        assert!(!r.sql.to_uppercase().contains(" GLOB "));
    }

    #[test]
    fn keyword_indexed_by_removed() {
        let r = translate("SELECT * FROM metadata_items INDEXED BY idx_title WHERE title = 'test'")
            .unwrap();
        assert!(!r.sql.to_uppercase().contains("INDEXED BY"));
        assert!(r.sql.to_uppercase().contains("WHERE"));
    }

    #[test]
    fn keyword_not_indexed_removed() {
        let r = translate("SELECT * FROM metadata_items NOT INDEXED WHERE id = 1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("NOT INDEXED"), "{}", r.sql);
        assert!(up.contains("WHERE"), "{}", r.sql);
    }

    #[test]
    fn keyword_sqlite_master_replaced() {
        let r = translate("SELECT name FROM sqlite_master WHERE type='table'").unwrap();
        assert!(
            r.sql.to_lowercase().contains("information_schema")
                || r.sql.to_lowercase().contains("pg_")
        );
        assert!(!r.sql.to_lowercase().contains("sqlite_master"));
    }

    #[test]
    fn keyword_empty_in_list() {
        let r = translate("SELECT * FROM tags WHERE id IN ()").unwrap();
        assert!(!r.sql.contains("IN ()"));
        assert!(r.sql.to_uppercase().contains("IN") && r.sql.to_uppercase().contains("SELECT"));
    }

    #[test]
    fn keyword_group_by_null_removed() {
        let r = translate("SELECT count(*) FROM metadata_items GROUP BY NULL").unwrap();
        assert!(!r.sql.to_uppercase().contains("GROUP BY NULL"));
    }

    #[test]
    fn keyword_pragma_read_is_mapped_to_select() {
        let r = translate("PRAGMA foreign_keys").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SELECT 1 AS FOREIGN_KEYS"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_set_is_mapped_to_select_one() {
        let r = translate("PRAGMA journal_mode=WAL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.JOURNAL_MODE"), "{}", r.sql);
        assert!(!up.contains("PRAGMA"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_schema_prefix_is_supported() {
        let r = translate("PRAGMA main.busy_timeout = 5000").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("LOCK_TIMEOUT"), "{}", r.sql);
        assert!(!up.contains("PRAGMA"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_unknown_is_removed() {
        let r = translate("PRAGMA this_is_unknown").unwrap();
        assert!(r.sql.trim().is_empty(), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_busy_timeout_read_uses_current_setting() {
        let r = translate("PRAGMA busy_timeout").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("LOCK_TIMEOUT"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_synchronous_set_uses_set_config() {
        let r = translate("PRAGMA synchronous = FULL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("SYNCHRONOUS_COMMIT"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_temp_store_set_uses_session_setting() {
        let r = translate("PRAGMA temp_store=MEMORY").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.TEMP_STORE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_temp_store_read_uses_current_setting() {
        let r = translate("PRAGMA temp_store").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.TEMP_STORE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_cache_size_set_uses_session_setting() {
        let r = translate("PRAGMA cache_size=-4000").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.CACHE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_cache_size_read_uses_current_setting() {
        let r = translate("PRAGMA cache_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.CACHE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_locking_mode_set_uses_session_setting() {
        let r = translate("PRAGMA locking_mode=EXCLUSIVE").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.LOCKING_MODE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_locking_mode_read_uses_current_setting() {
        let r = translate("PRAGMA locking_mode").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.LOCKING_MODE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_wal_autocheckpoint_set_uses_session_setting() {
        let r = translate("PRAGMA wal_autocheckpoint=200").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.WAL_AUTOCHECKPOINT"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_wal_autocheckpoint_read_uses_current_setting() {
        let r = translate("PRAGMA wal_autocheckpoint").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.WAL_AUTOCHECKPOINT"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_mmap_size_set_uses_session_setting() {
        let r = translate("PRAGMA mmap_size=1048576").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.MMAP_SIZE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_mmap_size_read_uses_current_setting() {
        let r = translate("PRAGMA mmap_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.MMAP_SIZE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_page_size_set_uses_session_setting() {
        let r = translate("PRAGMA page_size=8192").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.PAGE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_page_size_read_uses_current_setting() {
        let r = translate("PRAGMA page_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.PAGE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_auto_vacuum_set_uses_session_setting() {
        let r = translate("PRAGMA auto_vacuum=INCREMENTAL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.AUTO_VACUUM"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_auto_vacuum_read_uses_current_setting() {
        let r = translate("PRAGMA auto_vacuum").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.AUTO_VACUUM"), "{}", r.sql);
    }

    #[test]
    fn keyword_pragma_strict_mode_causes_translation_failure_for_unknown() {
        let _guard = env_lock().lock().unwrap();
        TEST_STRICT_PRAGMA_OVERRIDE.store(1, Ordering::Relaxed);
        let result = translate("PRAGMA totally_unknown_pragma");
        TEST_STRICT_PRAGMA_OVERRIDE.store(-1, Ordering::Relaxed);
        assert!(result.is_err(), "strict pragma mode should fail unknown PRAGMA");
    }

    #[test]
    fn keyword_explain_query_plan_rewritten_to_explain() {
        let r = translate("EXPLAIN QUERY PLAN SELECT * FROM t").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.starts_with("EXPLAIN "), "{}", r.sql);
        assert!(!up.contains("QUERY PLAN"), "{}", r.sql);
    }

    #[test]
    fn keyword_vacuum_rewritten_to_select_one() {
        let r = translate("VACUUM").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn keyword_reindex_rewritten_to_select_one() {
        let r = translate("REINDEX").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn keyword_attach_database_rewritten_to_select_one() {
        let r = translate("ATTACH DATABASE 'x.db' AS aux").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn keyword_detach_database_rewritten_to_select_one() {
        let r = translate("DETACH DATABASE aux").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn keyword_analyze_sqlite_internal_rewritten_to_select_one() {
        let r = translate("ANALYZE sqlite_master").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn keyword_create_table_without_rowid_strict_stripped() {
        let r = translate("CREATE TABLE t(id INTEGER PRIMARY KEY) WITHOUT ROWID, STRICT").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("WITHOUT ROWID"), "{}", r.sql);
        assert!(!up.contains("STRICT"), "{}", r.sql);
    }

    #[test]
    fn keyword_regexp_operator_rewritten() {
        let r = translate("SELECT * FROM t WHERE name REGEXP 'ab.*'").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("REGEXP"), "{}", r.sql);
        assert!(r.sql.contains('~'), "{}", r.sql);
    }

    #[test]
    fn keyword_not_regexp_operator_rewritten() {
        let r = translate("SELECT * FROM t WHERE name NOT REGEXP 'ab.*'").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("REGEXP"), "{}", r.sql);
        assert!(r.sql.contains("!~"), "{}", r.sql);
    }

    #[test]
    fn keyword_raise_function_rewritten_to_null() {
        let r = translate(
            "CREATE TRIGGER tr_bi BEFORE INSERT ON t BEGIN SELECT RAISE(ABORT, 'boom'); END",
        )
        .unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("RAISE("), "{}", r.sql);
        assert!(up.contains("NULL"), "{}", r.sql);
    }

    // ── COLLATE stripping tests ──────────────────────────────────────────────

    #[test]
    fn preprocess_strips_icu_root_in_order_by() {
        // This is the exact pattern that was causing parse failures in production
        let sql = "select metadata_items.id from metadata_items where metadata_items.library_section_id in (1) order by metadata_items.added_at desc, metadata_items.title_sort collate icu_root asc, metadata_items.id asc";
        let r = translate(sql).unwrap();
        let out = r.sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "icu_root collation should be stripped, got: {}",
            r.sql
        );
        assert!(
            out.contains("order by"),
            "ORDER BY should still be present, got: {}",
            r.sql
        );
    }

    #[test]
    fn preprocess_strips_icu_root_multiple_order_by_cols() {
        // Multiple COLLATE icu_root in same ORDER BY (from production query)
        let sql = "select id from metadata_items order by added_at desc, grandparents.title_sort collate icu_root asc, metadata_items.title_sort collate icu_root asc, metadata_items.id asc";
        let r = translate(sql).unwrap();
        let out = r.sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "All icu_root collations should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn preprocess_nocase_handled_at_ast_level() {
        // COLLATE NOCASE is left by pre-parse stripping; the AST-level handler in
        // query.rs converts standalone NOCASE in ORDER BY to LOWER(expr).
        let sql = "SELECT * FROM t ORDER BY name COLLATE NOCASE ASC";
        let r = translate(sql).unwrap();
        let out = r.sql.to_uppercase();
        // The final output should not contain raw COLLATE NOCASE
        assert!(
            !out.contains("COLLATE NOCASE"),
            "COLLATE NOCASE should be handled (converted to LOWER or stripped), got: {}",
            r.sql
        );
    }

    #[test]
    fn preprocess_strips_rtrim_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE RTRIM";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE RTRIM"),
            "RTRIM collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn preprocess_strips_binary_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE BINARY";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE BINARY"),
            "BINARY collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn preprocess_strips_unicode_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE UNICODE";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE UNICODE"),
            "UNICODE collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn preprocess_collate_not_stripped_inside_string() {
        // A string literal containing 'COLLATE icu_root' must not be touched
        let sql = "SELECT 'COLLATE icu_root' FROM t";
        let r = translate(sql).unwrap();
        assert!(
            r.sql.contains("COLLATE icu_root"),
            "Collate inside string literal should not be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn preprocess_long_query_collate_icu_parse_succeeds() {
        // Regression test: long query with COLLATE icu_root used to fail at parse time
        let sql = concat!(
            "select metadata_items.id from metadata_items ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id ",
            "where metadata_items.library_section_id in (2) ",
            "and (metadata_items.metadata_type=4 and metadata_items.added_at>1000000) ",
            "order by metadata_items.added_at desc, ",
            "grandparents.title_sort collate icu_root asc, ",
            "parents.`index` IS NULL, parents.`index` asc, ",
            "metadata_items.`index` IS NULL, metadata_items.`index` asc, ",
            "metadata_items.title_sort collate icu_root asc, ",
            "metadata_items.id asc"
        );
        let r = translate(sql);
        assert!(
            r.is_ok(),
            "Long query with COLLATE icu_root should parse successfully, got: {:?}",
            r.err()
        );
        let out = r.unwrap().sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "icu_root collation should be stripped from output, got: {}",
            out
        );
    }

    #[test]
    fn preprocess_does_not_treat_backtick_limit_identifier_as_limit_clause() {
        let sql = "SELECT pqg.`id`,pqg.`limit`,pqg.`continuous` FROM play_queue_generators pqg WHERE pqg.`type`!=:C1";
        let out = preprocess(sql);
        let low = out.to_ascii_lowercase();
        assert!(
            !low.contains(" offset "),
            "identifier `limit` should not trigger LIMIT/OFFSET rewrite: {}",
            out
        );
        assert!(
            out.contains("`limit`"),
            "backtick identifier should remain unchanged in preprocess output: {}",
            out
        );
    }

}
