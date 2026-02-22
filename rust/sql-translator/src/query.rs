/// Module: query
///
/// Miscellaneous query fixups for SQLite → PostgreSQL translation:
///   1. Subquery alias      — FROM (SELECT …) without alias → AS _subq
///   2. NULLS LAST          — `ORDER BY col IS NULL, col ASC` → `ORDER BY col NULLS LAST`
///   3. DISTINCT + agg ORDER BY — `SELECT DISTINCT … ORDER BY count(*)` → remove DISTINCT
///   4. CASE booleans       — `CASE WHEN x THEN 1 ELSE 0 END` → TRUE/FALSE
///   5. WHERE 1 / WHERE 0   — → WHERE TRUE / WHERE FALSE
///   6. max(a,b) multi-arg  — → GREATEST(a,b)
///   7. min(a,b) multi-arg  — → LEAST(a,b)
///   8. COLLATE ICU strip   — remove icu_* collations
///   9. COLLATE NOCASE      — `x COLLATE NOCASE = y` → `LOWER(x) = LOWER(y)`
///  10. FTS4 MATCH          — `col MATCH 'term'` → `col_fts @@ to_tsquery('simple', E'term')`
///  11. Collections filter  — remove `metadata_type=18` from OR conditions
///  12. Int/text mismatch   — cast integer to ::text for known text columns
///                            and cast string literal to integer for known int columns
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;

pub fn transform(stmt: &mut Statement) {
    match stmt {
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

fn transform_query(q: &mut Query) {
    if let Some(with) = &mut q.with {
        for cte in &mut with.cte_tables {
            transform_query(&mut cte.query);
        }
    }

    // Check for DISTINCT + aggregate in ORDER BY → remove DISTINCT
    fix_distinct_with_agg_orderby(q);

    transform_set_expr(&mut q.body);

    // Transform expressions in ORDER BY (collate stripping, etc.)
    if let Some(ob) = &mut q.order_by {
        if let OrderByKind::Expressions(exprs) = &mut ob.kind {
            for oe in exprs.iter_mut() {
                transform_expr_inplace(&mut oe.expr);
            }
        }
    }

    // Fix ORDER BY IS NULL pattern → NULLS LAST (must run after expr transform)
    fix_order_by_is_null(q);

    // Remove ORDER BY rowid (PostgreSQL doesn't have rowid)
    fix_order_by_rowid(q);
}

fn transform_set_expr(se: &mut SetExpr) {
    match se {
        SetExpr::Select(s) => transform_select(s),
        SetExpr::Query(q) => transform_query(q),
        SetExpr::SetOperation { left, right, .. } => {
            transform_set_expr(left);
            transform_set_expr(right);
        }
        SetExpr::Values(vals) => {
            for row in &mut vals.rows {
                for e in row {
                    transform_expr_inplace(e);
                }
            }
        }
        _ => {}
    }
}

fn transform_select(sel: &mut Select) {
    // Fix FROM subqueries without alias
    for twj in &mut sel.from {
        fix_table_with_joins(twj);
    }

    // WHERE fixups
    if let Some(w) = &mut sel.selection {
        fix_where_numeric(w);
        fix_collections_filter(w);
        transform_expr_inplace(w);
    }

    // Projection
    for item in &mut sel.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                transform_expr_inplace(e);
            }
            _ => {}
        }
    }

    // GROUP BY
    if let GroupByExpr::Expressions(ref mut exprs, _) = sel.group_by {
        for e in exprs {
            transform_expr_inplace(e);
        }
    }

    // HAVING
    if let Some(h) = &mut sel.having {
        transform_expr_inplace(h);
    }
}

// ─── Fix 1: Subquery alias ────────────────────────────────────────────────────

fn fix_table_with_joins(twj: &mut TableWithJoins) {
    fix_table_factor(&mut twj.relation);
    for join in &mut twj.joins {
        fix_table_factor(&mut join.relation);
    }
}

fn fix_table_factor(tf: &mut TableFactor) {
    if let TableFactor::Derived {
        subquery, alias, ..
    } = tf
    {
        transform_query(subquery);
        // Add alias if missing
        if alias.is_none() {
            *alias = Some(TableAlias {
                explicit: true,
                name: Ident::new("_subq"),
                columns: vec![],
            });
        }
    }
}

// ─── Fix 2: NULLS LAST from IS NULL pattern ───────────────────────────────────

/// Detect: `ORDER BY expr IS NULL, expr [ASC|DESC]`
/// Replace with: `ORDER BY expr [ASC|DESC] NULLS LAST`
fn fix_order_by_is_null(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };

    // Find pairs: (i, j) where exprs[i] is `x IS NULL` and exprs[j] is `x [ASC|DESC]`
    // Strategy: collect indices of IS NULL exprs, match with following expr
    let len = exprs.len();
    if len < 2 {
        return;
    }

    let mut to_remove: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < len {
        if let Expr::IsNull(ref inner) = exprs[i].expr {
            let col = *inner.clone();
            // Look for the next expr that matches `col`
            if i + 1 < len {
                let next = &mut exprs[i + 1];
                if exprs_display_eq(&next.expr, &col) {
                    // Set NULLS LAST on the real ordering expr
                    next.options.nulls_first = Some(false);
                    to_remove.push(i);
                    i += 2;
                    continue;
                }
            }
        }
        i += 1;
    }

    // Remove IS NULL entries in reverse order
    for idx in to_remove.into_iter().rev() {
        exprs.remove(idx);
    }
}

fn exprs_display_eq(a: &Expr, b: &Expr) -> bool {
    format!("{}", a).to_lowercase() == format!("{}", b).to_lowercase()
}

// ─── Fix 11: ORDER BY rowid → removed ────────────────────────────────────────

fn fix_order_by_rowid(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };

    // Remove any ORDER BY expression that is just `rowid`
    exprs.retain(|oe| {
        if let Expr::Identifier(ident) = &oe.expr {
            ident.value.to_lowercase() != "rowid"
        } else {
            true
        }
    });

    // If all ORDER BY expressions were removed, remove the ORDER BY clause
    if exprs.is_empty() {
        q.order_by = None;
    }
}

// ─── Fix 3: DISTINCT + aggregate ORDER BY ─────────────────────────────────────

fn fix_distinct_with_agg_orderby(q: &mut Query) {
    let has_distinct = if let SetExpr::Select(sel) = q.body.as_ref() {
        matches!(sel.distinct, Some(Distinct::Distinct))
    } else {
        false
    };

    if !has_distinct {
        return;
    }

    let has_incompatible_in_order = if let Some(ob) = &q.order_by {
        if let OrderByKind::Expressions(exprs) = &ob.kind {
            exprs
                .iter()
                .any(|oe| is_distinct_incompatible_expr(&oe.expr))
        } else {
            false
        }
    } else {
        false
    };

    if has_incompatible_in_order {
        if let SetExpr::Select(sel) = q.body.as_mut() {
            sel.distinct = None;
        }
    }
}

// ─── Fix 5: WHERE 1 / WHERE 0 ─────────────────────────────────────────────────

fn fix_where_numeric(expr: &mut Expr) {
    if let Expr::Value(ValueWithSpan {
        value: Value::Number(ref s, _),
        ref span,
    }) = *expr
    {
        if s == "1" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(true),
                span: *span,
            });
        } else if s == "0" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(false),
                span: *span,
            });
        }
    }
}

// ─── Main expression transformer ─────────────────────────────────────────────

fn transform_expr(expr: Expr) -> Expr {
    match expr {
        // Fix 4: CASE WHEN x THEN 1 ELSE 0 END → TRUE/FALSE
        Expr::Case {
            case_token,
            end_token,
            operand,
            conditions,
            else_result,
        } => {
            let conditions: Vec<CaseWhen> = conditions
                .into_iter()
                .map(|cw| CaseWhen {
                    condition: transform_expr(cw.condition),
                    result: maybe_bool_value(transform_expr(cw.result)),
                })
                .collect();
            let else_result = else_result.map(|e| Box::new(maybe_bool_value(transform_expr(*e))));
            Expr::Case {
                case_token,
                end_token,
                operand: operand.map(|o| Box::new(transform_expr(*o))),
                conditions,
                else_result,
            }
        }

        // Fix 6 & 7: max(a,b) → GREATEST, min(a,b) → LEAST
        // Fix 8 & 9: COLLATE handling
        Expr::Function(mut func) => {
            // Get name and arg count before mutable borrow
            let fn_name = func_name_str(&func);
            let arg_count = if let FunctionArguments::List(ref al) = func.args {
                al.args.len()
            } else {
                0
            };

            // Recurse into args
            if let FunctionArguments::List(ref mut al) = func.args {
                for arg in &mut al.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            transform_expr_inplace(e);
                        }
                        FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            transform_expr_inplace(e);
                        }
                        _ => {}
                    }
                }
            }

            if fn_name == "max" && arg_count >= 2 {
                rename_function(&mut func, "GREATEST");
            } else if fn_name == "min" && arg_count >= 2 {
                rename_function(&mut func, "LEAST");
            }

            Expr::Function(func)
        }

        // Fix FTS4: col MATCH 'term' → col_fts @@ to_tsquery('simple', E'converted_term')
        Expr::BinaryOp {
            left,
            op: BinaryOperator::PGCustomBinaryOperator(ref custom_ops),
            right,
        } if custom_ops.len() == 1 && custom_ops[0] == "MATCH" => {
            transform_fts_match(*left, *right)
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Match,
            right,
        } => transform_fts_match(*left, *right),

        // Fix 12: Int/text mismatch — known text columns compared with int literals
        // and known int columns compared with string literals
        Expr::BinaryOp {
            ref left,
            ref op,
            ref right,
        } if matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq)
            && should_fix_int_text_mismatch(left, right) =>
        {
            let (left, op, right) = match expr {
                Expr::BinaryOp { left, op, right } => (*left, op, *right),
                _ => unreachable!(),
            };
            fix_int_text_mismatch(left, op, right)
        }

        // Fix 9: COLLATE NOCASE on equality → LOWER(x) = LOWER(y)
        // Fix 10: 0/1 in boolean context (AND/OR) → FALSE/TRUE
        // Fix general BinaryOp recursion
        Expr::BinaryOp { left, op, right } => {
            // Before recursing, check for COLLATE NOCASE at this level
            let is_eq_op = matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq);
            let is_logical_op = matches!(op, BinaryOperator::And | BinaryOperator::Or);
            let (left_stripped, left_nocase) = strip_collate_nocase_shallow(*left);
            let (right_stripped, right_nocase) = strip_collate_nocase_shallow(*right);

            if is_eq_op && (left_nocase || right_nocase) {
                // Recurse into the stripped inner expressions
                let left_t = transform_expr(left_stripped);
                let right_t = transform_expr(right_stripped);
                Expr::BinaryOp {
                    left: Box::new(wrap_lower(left_t)),
                    op,
                    right: Box::new(wrap_lower(right_t)),
                }
            } else if is_logical_op {
                // In AND/OR context, convert bare 0/1 to FALSE/TRUE
                let left_t = maybe_bool_value(transform_expr(left_stripped));
                let right_t = maybe_bool_value(transform_expr(right_stripped));
                Expr::BinaryOp {
                    left: Box::new(left_t),
                    op,
                    right: Box::new(right_t),
                }
            } else {
                // Put back what we stripped (if nothing was NOCASE, strip_collate returns original)
                let left_t = transform_expr(left_stripped);
                let right_t = transform_expr(right_stripped);
                Expr::BinaryOp {
                    left: Box::new(left_t),
                    op,
                    right: Box::new(right_t),
                }
            }
        }
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(transform_expr(*expr)),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(transform_expr(*inner))),
        Expr::Cast {
            kind,
            expr,
            data_type,
            array,
            format,
        } => Expr::Cast {
            kind,
            expr: Box::new(transform_expr(*expr)),
            data_type,
            array,
            format,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(transform_expr(*expr)),
            list: list.into_iter().map(transform_expr).collect(),
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
        Expr::Collate { expr, collation } => {
            // Fix 8: strip ICU collations entirely
            // Fix 9b: NOCASE outside equality → wrap in LOWER()
            // Note: NOCASE on equality is handled in the BinaryOp arm above
            let collation_name = collation
                .0
                .first()
                .and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
                .unwrap_or_default();

            let inner = transform_expr(*expr);

            if collation_name.starts_with("icu") || collation_name == "unicode" {
                // Strip the collation entirely
                inner
            } else if collation_name == "nocase" {
                // Standalone NOCASE (ORDER BY, non-equality) → LOWER(expr)
                wrap_lower(inner)
            } else {
                // Keep other collations
                Expr::Collate {
                    expr: Box::new(inner),
                    collation,
                }
            }
        }
        // Fix LIKE ... COLLATE NOCASE → ILIKE
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
            any,
        } => {
            let (pat_inner, pat_nocase) = strip_collate_nocase_shallow(*pattern);
            let expr_t = Box::new(transform_expr(*expr));
            let pat_t = Box::new(transform_expr(pat_inner));
            if pat_nocase {
                // Convert to ILIKE
                Expr::ILike {
                    negated,
                    expr: expr_t,
                    pattern: pat_t,
                    escape_char,
                    any,
                }
            } else {
                Expr::Like {
                    negated,
                    expr: expr_t,
                    pattern: pat_t,
                    escape_char,
                    any,
                }
            }
        }
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
            any,
        } => {
            // Strip NOCASE from ILIKE too (already case-insensitive)
            let (pat_inner, _) = strip_collate_nocase_shallow(*pattern);
            Expr::ILike {
                negated,
                expr: Box::new(transform_expr(*expr)),
                pattern: Box::new(transform_expr(pat_inner)),
                escape_char,
                any,
            }
        }
        Expr::IsNull(inner) => Expr::IsNull(Box::new(transform_expr(*inner))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(transform_expr(*inner))),
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

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn func_name_str(func: &Function) -> String {
    func.name
        .0
        .first()
        .and_then(|p| match p {
            ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
            _ => None,
        })
        .unwrap_or_default()
}

fn rename_function(func: &mut Function, new_name: &str) {
    func.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(new_name))]);
}

fn is_distinct_incompatible_expr(expr: &Expr) -> bool {
    // Functions that are incompatible with DISTINCT (aggregates + random)
    if let Expr::Function(func) = expr {
        let name = func_name_str(func);
        name == "random" || is_aggregate_expr(expr)
    } else {
        false
    }
}

fn is_aggregate_expr(expr: &Expr) -> bool {
    const AGGREGATES: &[&str] = &[
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "group_concat",
        "string_agg",
        "array_agg",
        "bool_and",
        "bool_or",
    ];
    if let Expr::Function(func) = expr {
        let name = func_name_str(func);
        AGGREGATES.contains(&name.as_str())
    } else {
        false
    }
}

/// If the expression is a numeric literal 1 or 0, replace with TRUE/FALSE.
fn maybe_bool_value(expr: Expr) -> Expr {
    if let Expr::Value(ValueWithSpan {
        value: Value::Number(ref s, _),
        ref span,
    }) = expr
    {
        if s == "1" {
            return Expr::Value(ValueWithSpan {
                value: Value::Boolean(true),
                span: *span,
            });
        } else if s == "0" {
            return Expr::Value(ValueWithSpan {
                value: Value::Boolean(false),
                span: *span,
            });
        }
    }
    expr
}

/// Strip COLLATE NOCASE from an expression (shallow — does not recurse).
/// Returns (inner_expr, was_nocase).
fn strip_collate_nocase_shallow(expr: Expr) -> (Expr, bool) {
    if let Expr::Collate {
        expr: inner,
        collation,
    } = expr
    {
        let collation_name = collation
            .0
            .first()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .unwrap_or_default();

        if collation_name == "nocase" {
            return (*inner, true);
        } else if collation_name.starts_with("icu") || collation_name == "unicode" {
            // Strip ICU collation, not NOCASE
            return (*inner, false);
        }
        return (
            Expr::Collate {
                expr: inner,
                collation,
            },
            false,
        );
    }
    (expr, false)
}

// ─── Fix 11: Collections filter ───────────────────────────────────────────────

/// Remove `metadata_type=18` from OR conditions.
/// Plex can't properly serialize collection objects (type 18), causing errors.
/// Pattern: `(metadata_type=1 OR metadata_type=18)` → `metadata_type=1`
fn fix_collections_filter(expr: &mut Expr) {
    // Recurse into nested expressions first
    match expr {
        Expr::Nested(inner) => {
            fix_collections_filter(inner);
            // After fixing inner, if it simplified to a single condition, unwrap
            if is_metadata_type_eq(inner, 18) {
                // Entire expression is just metadata_type=18 — replace with TRUE
                *expr = Expr::Value(ValueWithSpan {
                    value: Value::Boolean(true),
                    span: Span::empty(),
                });
            }
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            fix_collections_filter(left);
            fix_collections_filter(right);

            let left_is_18 = is_metadata_type_eq(left, 18);
            let right_is_18 = is_metadata_type_eq(right, 18);

            if left_is_18 && !right_is_18 {
                // Remove left (type=18), keep right
                let r = std::mem::replace(
                    right.as_mut(),
                    Expr::Value(ValueWithSpan {
                        value: Value::Null,
                        span: Span::empty(),
                    }),
                );
                *expr = r;
            } else if right_is_18 && !left_is_18 {
                // Remove right (type=18), keep left
                let l = std::mem::replace(
                    left.as_mut(),
                    Expr::Value(ValueWithSpan {
                        value: Value::Null,
                        span: Span::empty(),
                    }),
                );
                *expr = l;
            }
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            fix_collections_filter(left);
            fix_collections_filter(right);
        }
        _ => {}
    }
}

/// Check if expr matches `metadata_type = <value>` or `metadata_items.metadata_type = <value>`
fn is_metadata_type_eq(expr: &Expr, value: i64) -> bool {
    if let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    {
        let col_name = match left.as_ref() {
            Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
            Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
            _ => None,
        };
        if col_name.as_deref() == Some("metadata_type") {
            if let Expr::Value(ValueWithSpan {
                value: Value::Number(ref s, _),
                ..
            }) = right.as_ref()
            {
                if let Ok(n) = s.parse::<i64>() {
                    return n == value;
                }
            }
        }
    }
    false
}

// ─── Fix 12: Int/text mismatch ────────────────────────────────────────────────

/// Known columns that are stored as TEXT in Plex but sometimes compared with integers
const KNOWN_TEXT_COLUMNS: &[&str] = &["status", "state", "downloaded", "metadata_item_id"];

/// Known columns that are stored as INTEGER in Plex but sometimes compared with strings
const KNOWN_INT_COLUMNS: &[&str] = &["id"];

fn get_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
        _ => None,
    }
}

fn is_number_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::Number(_, _),
            ..
        })
    )
}

fn is_string_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(_),
            ..
        })
    )
}

/// Check if this binary comparison has an int/text mismatch that needs fixing
fn should_fix_int_text_mismatch(left: &Expr, right: &Expr) -> bool {
    // Pattern A: known_text_col = number → cast number to text
    if let Some(col) = get_column_name(left) {
        if KNOWN_TEXT_COLUMNS.contains(&col.as_str()) && is_number_literal(right) {
            return true;
        }
    }
    if let Some(col) = get_column_name(right) {
        if KNOWN_TEXT_COLUMNS.contains(&col.as_str()) && is_number_literal(left) {
            return true;
        }
    }
    // Pattern B: known_int_col = 'string' → cast string to int
    if let Some(col) = get_column_name(left) {
        if KNOWN_INT_COLUMNS.contains(&col.as_str()) && is_string_literal(right) {
            return true;
        }
    }
    if let Some(col) = get_column_name(right) {
        if KNOWN_INT_COLUMNS.contains(&col.as_str()) && is_string_literal(left) {
            return true;
        }
    }
    false
}

/// Fix int/text mismatch by adding appropriate casts
fn fix_int_text_mismatch(left: Expr, op: BinaryOperator, right: Expr) -> Expr {
    let left_col = get_column_name(&left);
    let right_col = get_column_name(&right);

    let (new_left, new_right) = if left_col
        .as_ref()
        .map(|c| KNOWN_TEXT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_number_literal(&right)
    {
        // text_col = 123 → text_col = 123::text
        (
            transform_expr(left),
            Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(transform_expr(right)),
                data_type: DataType::Text,
                array: false,
                format: None,
            },
        )
    } else if right_col
        .as_ref()
        .map(|c| KNOWN_TEXT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_number_literal(&left)
    {
        (
            Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(transform_expr(left)),
                data_type: DataType::Text,
                array: false,
                format: None,
            },
            transform_expr(right),
        )
    } else if left_col
        .as_ref()
        .map(|c| KNOWN_INT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_string_literal(&right)
    {
        // int_col = '123' → int_col = CAST('123' AS INTEGER)
        (
            transform_expr(left),
            Expr::Cast {
                kind: CastKind::Cast,
                expr: Box::new(transform_expr(right)),
                data_type: DataType::Integer(None),
                array: false,
                format: None,
            },
        )
    } else if right_col
        .as_ref()
        .map(|c| KNOWN_INT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_string_literal(&left)
    {
        (
            Expr::Cast {
                kind: CastKind::Cast,
                expr: Box::new(transform_expr(left)),
                data_type: DataType::Integer(None),
                array: false,
                format: None,
            },
            transform_expr(right),
        )
    } else {
        (transform_expr(left), transform_expr(right))
    };

    Expr::BinaryOp {
        left: Box::new(new_left),
        op,
        right: Box::new(new_right),
    }
}

// ─── Fix FTS4: MATCH → @@ to_tsquery ──────────────────────────────────────────

/// Transform `col MATCH 'term'` → `col_fts @@ to_tsquery('simple', E'converted_term')`
fn transform_fts_match(left: Expr, right: Expr) -> Expr {
    // Get the FTS column name and append _fts
    let fts_col = match &left {
        Expr::Identifier(ident) => Expr::Identifier(Ident::new(format!("{}_fts", ident.value))),
        Expr::CompoundIdentifier(parts) => {
            let mut new_parts = parts.clone();
            if let Some(last) = new_parts.last_mut() {
                last.value = format!("{}_fts", last.value);
            }
            Expr::CompoundIdentifier(new_parts)
        }
        _ => left,
    };

    // Convert the match term to tsquery syntax
    let term_str = match &right {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            ..
        }) => convert_fts_term(s),
        _ => {
            return Expr::BinaryOp {
                left: Box::new(fts_col),
                op: BinaryOperator::AtAt,
                right: Box::new(right),
            }
        }
    };

    // Build: col_fts @@ to_tsquery('simple', E'term')
    let tsquery_call = Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("to_tsquery"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                    value: Value::SingleQuotedString("simple".to_string()),
                    span: Span::empty(),
                }))),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                    value: Value::EscapedStringLiteral(term_str),
                    span: Span::empty(),
                }))),
            ],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    });

    Expr::BinaryOp {
        left: Box::new(fts_col),
        op: BinaryOperator::AtAt,
        right: Box::new(tsquery_call),
    }
}

/// Convert SQLite FTS term syntax to PostgreSQL tsquery syntax.
///
/// | SQLite         | PostgreSQL tsquery |
/// |----------------|-------------------|
/// | `term`         | `term`            |
/// | `term1 term2`  | `term1 & term2`   |
/// | `-term`        | `!term`           |
/// | `term1 OR term2`| `term1 | term2`  |
/// | `"exact phrase"`| `exact <-> phrase`|
/// | `'` (quote)    | stripped           |
fn convert_fts_term(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    let mut need_and = false;

    while let Some(&c) = chars.peek() {
        match c {
            // Negation
            '-' => {
                if need_and {
                    result.push_str(" & ");
                }
                chars.next();
                result.push('!');
                need_and = false;
            }
            // Phrase: "word1 word2" → word1 <-> word2
            '"' => {
                if need_and {
                    result.push_str(" & ");
                }
                chars.next(); // skip opening "
                let mut phrase = String::new();
                while let Some(&pc) = chars.peek() {
                    if pc == '"' {
                        chars.next();
                        break;
                    }
                    phrase.push(pc);
                    chars.next();
                }
                let words: Vec<&str> = phrase.split_whitespace().collect();
                result.push_str(&words.join(" <-> "));
                need_and = true;
            }
            // Single quote — strip (not valid in tsquery)
            '\'' => {
                chars.next();
            }
            // Whitespace — check for OR or implicit AND
            ' ' | '\t' | '\n' => {
                chars.next();
                // Skip whitespace
                while let Some(&wc) = chars.peek() {
                    if wc == ' ' || wc == '\t' || wc == '\n' {
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Check for OR keyword
                if chars.peek() == Some(&'O') || chars.peek() == Some(&'o') {
                    let rest: String = chars.clone().take(3).collect();
                    if rest.to_uppercase().starts_with("OR ")
                        || (rest.len() >= 2 && rest[..2].to_uppercase() == "OR" && rest.len() == 2)
                    {
                        chars.next(); // O
                        chars.next(); // R
                                      // skip space after OR
                        while let Some(&wc) = chars.peek() {
                            if wc == ' ' {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        result.push_str(" | ");
                        need_and = false;
                        continue;
                    }
                }
                // Implicit AND (space between terms)
                if need_and && chars.peek().is_some() {
                    result.push_str(" & ");
                    need_and = false;
                }
            }
            // Regular character — part of a term
            _ => {
                if !need_and {
                    // First char of a new term
                }
                result.push(c);
                chars.next();
                need_and = true;
            }
        }
    }
    result
}

/// Wrap an expression in LOWER(…)
fn wrap_lower(expr: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("LOWER"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::translate;

    #[test]
    fn query_subquery_gets_alias() {
        let r = translate("SELECT * FROM (SELECT id FROM t) WHERE id > 0").unwrap();
        let sql_up = r.sql.to_uppercase();
        assert!(
            sql_up.contains("AS ") || sql_up.contains(" AS"),
            "Subquery should have alias, got: {}",
            r.sql
        );
    }

    #[test]
    fn query_nulls_last_from_is_null_pattern() {
        let r =
            translate(r#"SELECT * FROM t ORDER BY parents."index" IS NULL, parents."index" asc"#)
                .unwrap();
        let sql = r.sql.to_uppercase();
        assert!(
            sql.contains("NULLS LAST"),
            "Expected NULLS LAST, got: {}",
            r.sql
        );
        assert!(
            !sql.contains("IS NULL"),
            "IS NULL pattern should be replaced, got: {}",
            r.sql
        );
    }

    #[test]
    fn query_case_then_1_else_0_to_booleans() {
        let r = translate("SELECT (CASE WHEN a THEN 1 ELSE 0 END) FROM t").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("true") || sql.contains("false"),
            "Expected boolean values, got: {}",
            r.sql
        );
    }

    #[test]
    fn query_where_0_to_false() {
        let r = translate("SELECT * FROM t WHERE 0").unwrap();
        assert!(r.sql.to_uppercase().contains("FALSE"), "Got: {}", r.sql);
    }

    #[test]
    fn query_where_1_to_true() {
        let r = translate("SELECT * FROM t WHERE 1").unwrap();
        assert!(r.sql.to_uppercase().contains("TRUE"), "Got: {}", r.sql);
    }

    #[test]
    fn query_max_two_args_to_greatest() {
        let r = translate("SELECT max(x, y) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("GREATEST"), "Got: {}", r.sql);
    }

    #[test]
    fn query_max_one_arg_preserved() {
        let r = translate("SELECT max(x) FROM t").unwrap();
        assert!(!r.sql.to_uppercase().contains("GREATEST"), "Got: {}", r.sql);
        assert!(r.sql.to_lowercase().contains("max("), "Got: {}", r.sql);
    }

    #[test]
    fn query_min_two_args_to_least() {
        let r = translate("SELECT min(x, y) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("LEAST"), "Got: {}", r.sql);
    }

    #[test]
    fn query_strip_icu_collation() {
        let r = translate("SELECT * FROM t ORDER BY name COLLATE icu_root").unwrap();
        assert!(!r.sql.to_lowercase().contains("icu_root"), "Got: {}", r.sql);
    }

    #[test]
    fn query_collate_nocase_to_lower() {
        let r = translate("SELECT * FROM t WHERE name COLLATE NOCASE = 'Test'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(sql.contains("lower("), "Expected LOWER(), got: {}", r.sql);
        assert!(
            !sql.contains("collate nocase"),
            "COLLATE NOCASE should be removed, got: {}",
            r.sql
        );
    }

    #[test]
    fn query_orderby_unchanged_without_isnull() {
        let r = translate("SELECT * FROM t ORDER BY id").unwrap();
        assert!(
            !r.sql.to_uppercase().contains("NULLS LAST"),
            "Should not add NULLS LAST, got: {}",
            r.sql
        );
    }
}
