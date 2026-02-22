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
                transform_expr_inplace(sel);
            }
        }
        Statement::Delete(d) => {
            if let Some(sel) = &mut d.selection {
                transform_expr_inplace(sel);
            }
        }
        _ => {}
    }
}

// ─── Pre-parse string normalisation helpers ───────────────────────────────────

/// Normalise raw SQL before parsing:
///   - `GLOB 'pattern'` → `ILIKE 'pg_pattern'`  (wildcards: * → %, ? → _)
///   - `INDEXED BY <name>` → removed
///
/// Called from `crate::preprocess` (lib.rs) before the SQL hits sqlparser.
pub fn preprocess(sql: &str) -> String {
    let sql = fix_placeholder_spacing(sql);
    let sql = rewrite_glob(&sql);
    let sql = rewrite_indexed_by(&sql);
    sql
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
            // `?left` → `?` (strip the trailing identifier, it's junk from SQLite)
            result.push('?');
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
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

/// Remove `INDEXED BY <identifier>` hints.
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
            !exprs.is_empty() && exprs.iter().all(|e| is_null_expr(e))
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
        transform_expr_inplace(w);
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
                transform_expr_inplace(e);
            }
            _ => {}
        }
    }

    // HAVING
    if let Some(h) = &mut sel.having {
        transform_expr_inplace(h);
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

fn transform_expr(expr: Expr) -> Expr {
    match expr {
        // IN () → IN (SELECT -1 WHERE FALSE)
        Expr::InList {
            expr: inner,
            list,
            negated,
        } if list.is_empty() => {
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
            Expr::InSubquery {
                expr: inner,
                subquery: false_select,
                negated,
            }
        }
        // Recurse
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(transform_expr(*left)),
            op,
            right: Box::new(transform_expr(*right)),
        },
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(transform_expr(*expr)),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(transform_expr(*inner))),
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(transform_expr(*expr)),
            list: list.into_iter().map(transform_expr).collect(),
            negated,
        },
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(transform_expr(*expr)),
            subquery,
            negated,
        },
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(transform_expr(*expr)),
            negated,
            low: Box::new(transform_expr(*low)),
            high: Box::new(transform_expr(*high)),
        },
        Expr::Case {
            case_token,
            end_token,
            operand,
            conditions,
            else_result,
        } => Expr::Case {
            case_token,
            end_token,
            operand: operand.map(|o| Box::new(transform_expr(*o))),
            conditions: conditions
                .into_iter()
                .map(|cw| CaseWhen {
                    condition: transform_expr(cw.condition),
                    result: transform_expr(cw.result),
                })
                .collect(),
            else_result: else_result.map(|e| Box::new(transform_expr(*e))),
        },
        other => other,
    }
}

fn transform_expr_inplace(expr: &mut Expr) {
    let taken = std::mem::replace(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::Null,
            span: Span::empty(),
        }),
    );
    *expr = transform_expr(taken);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::translate;

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
}
