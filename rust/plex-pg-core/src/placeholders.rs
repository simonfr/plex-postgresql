/// Module: placeholders
///
/// Replaces `?` and `:name` placeholders with PostgreSQL-style `$1`, `$2`, ...
/// Builds a `param_names` vector: `None` for `?`, `Some("name")` for `:name`.
/// The same `:name` reuses the same `$N`.
use sqlparser::ast::*;

pub fn transform(stmt: &mut Statement, param_names: &mut Vec<Option<String>>) {
    // counter: next $N index (1-based)
    let mut counter: usize = param_names.len() + 1;
    // map: named placeholder → $N index
    let mut name_map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    transform_stmt(stmt, param_names, &mut counter, &mut name_map);
}

fn transform_stmt(
    stmt: &mut Statement,
    param_names: &mut Vec<Option<String>>,
    counter: &mut usize,
    name_map: &mut std::collections::HashMap<String, usize>,
) {
    match stmt {
        Statement::Query(q) => transform_query(q, param_names, counter, name_map),
        Statement::Insert(insert) => {
            // Transform the source query
            if let Some(src) = &mut insert.source {
                transform_query(src, param_names, counter, name_map);
            }
            // Transform ON CONFLICT DO UPDATE SET expressions
            if let Some(on_insert) = &mut insert.on {
                if let OnInsert::OnConflict(on_conflict) = on_insert {
                    if let OnConflictAction::DoUpdate(do_update) = &mut on_conflict.action {
                        for assign in &mut do_update.assignments {
                            transform_expr(&mut assign.value, param_names, counter, name_map);
                        }
                        if let Some(sel) = &mut do_update.selection {
                            transform_expr(sel, param_names, counter, name_map);
                        }
                    }
                }
            }
        }
        Statement::Update(update) => {
            for assign in &mut update.assignments {
                transform_expr(&mut assign.value, param_names, counter, name_map);
            }
            if let Some(sel) = &mut update.selection {
                transform_expr(sel, param_names, counter, name_map);
            }
        }
        Statement::Delete(del) => {
            if let Some(sel) = &mut del.selection {
                transform_expr(sel, param_names, counter, name_map);
            }
        }
        _ => {}
    }
}

fn transform_query(
    query: &mut Query,
    param_names: &mut Vec<Option<String>>,
    counter: &mut usize,
    name_map: &mut std::collections::HashMap<String, usize>,
) {
    // CTEs
    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            transform_query(&mut cte.query, param_names, counter, name_map);
        }
    }
    transform_set_expr(&mut query.body, param_names, counter, name_map);
    // ORDER BY
    if let Some(order_by) = &mut query.order_by {
        if let OrderByKind::Expressions(exprs) = &mut order_by.kind {
            for order_expr in exprs {
                transform_expr(&mut order_expr.expr, param_names, counter, name_map);
            }
        }
    }
    // LIMIT / OFFSET
    if let Some(limit_clause) = &mut query.limit_clause {
        match limit_clause {
            LimitClause::LimitOffset { limit, offset, .. } => {
                if let Some(l) = limit {
                    transform_expr(l, param_names, counter, name_map);
                }
                if let Some(o) = offset {
                    transform_expr(&mut o.value, param_names, counter, name_map);
                }
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                transform_expr(offset, param_names, counter, name_map);
                transform_expr(limit, param_names, counter, name_map);
            }
        }
    }
}

fn transform_set_expr(
    set_expr: &mut SetExpr,
    param_names: &mut Vec<Option<String>>,
    counter: &mut usize,
    name_map: &mut std::collections::HashMap<String, usize>,
) {
    match set_expr {
        SetExpr::Select(select) => transform_select(select, param_names, counter, name_map),
        SetExpr::Query(q) => transform_query(q, param_names, counter, name_map),
        SetExpr::SetOperation { left, right, .. } => {
            transform_set_expr(left, param_names, counter, name_map);
            transform_set_expr(right, param_names, counter, name_map);
        }
        SetExpr::Values(values) => {
            for row in &mut values.rows {
                for expr in row {
                    transform_expr(expr, param_names, counter, name_map);
                }
            }
        }
        _ => {}
    }
}

fn transform_select(
    select: &mut Select,
    param_names: &mut Vec<Option<String>>,
    counter: &mut usize,
    name_map: &mut std::collections::HashMap<String, usize>,
) {
    // SELECT items
    for item in &mut select.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                transform_expr(e, param_names, counter, name_map);
            }
            _ => {}
        }
    }
    // FROM
    for table in &mut select.from {
        transform_table_with_joins(table, param_names, counter, name_map);
    }
    // WHERE
    if let Some(sel) = &mut select.selection {
        transform_expr(sel, param_names, counter, name_map);
    }
    // GROUP BY
    match &mut select.group_by {
        GroupByExpr::Expressions(exprs, _) => {
            for e in exprs {
                transform_expr(e, param_names, counter, name_map);
            }
        }
        _ => {}
    }
    // HAVING
    if let Some(having) = &mut select.having {
        transform_expr(having, param_names, counter, name_map);
    }
}

fn transform_table_with_joins(
    table: &mut TableWithJoins,
    param_names: &mut Vec<Option<String>>,
    counter: &mut usize,
    name_map: &mut std::collections::HashMap<String, usize>,
) {
    transform_table_factor(&mut table.relation, param_names, counter, name_map);
    for join in &mut table.joins {
        transform_table_factor(&mut join.relation, param_names, counter, name_map);
        match &mut join.join_operator {
            JoinOperator::Join(JoinConstraint::On(e))
            | JoinOperator::Inner(JoinConstraint::On(e))
            | JoinOperator::Left(JoinConstraint::On(e))
            | JoinOperator::LeftOuter(JoinConstraint::On(e))
            | JoinOperator::Right(JoinConstraint::On(e))
            | JoinOperator::RightOuter(JoinConstraint::On(e))
            | JoinOperator::Semi(JoinConstraint::On(e))
            | JoinOperator::LeftSemi(JoinConstraint::On(e))
            | JoinOperator::RightSemi(JoinConstraint::On(e))
            | JoinOperator::Anti(JoinConstraint::On(e))
            | JoinOperator::LeftAnti(JoinConstraint::On(e))
            | JoinOperator::RightAnti(JoinConstraint::On(e))
            | JoinOperator::StraightJoin(JoinConstraint::On(e))
            | JoinOperator::FullOuter(JoinConstraint::On(e)) => {
                transform_expr(e, param_names, counter, name_map);
            }
            _ => {}
        }
    }
}

fn transform_table_factor(
    factor: &mut TableFactor,
    param_names: &mut Vec<Option<String>>,
    counter: &mut usize,
    name_map: &mut std::collections::HashMap<String, usize>,
) {
    match factor {
        TableFactor::Derived { subquery, .. } => {
            transform_query(subquery, param_names, counter, name_map);
        }
        TableFactor::Table { args, .. } => {
            if let Some(TableFunctionArgs { args: fn_args, .. }) = args {
                for arg in fn_args.iter_mut() {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            transform_expr(e, param_names, counter, name_map);
                        }
                        FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            transform_expr(e, param_names, counter, name_map);
                        }
                        _ => {}
                    }
                }
            }
        }
        TableFactor::Function { args, .. } => {
            for arg in args.iter_mut() {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        transform_expr(e, param_names, counter, name_map);
                    }
                    FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(e),
                        ..
                    } => {
                        transform_expr(e, param_names, counter, name_map);
                    }
                    _ => {}
                }
            }
        }
        TableFactor::TableFunction { expr, .. } => {
            transform_expr(expr, param_names, counter, name_map);
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            transform_table_with_joins(table_with_joins, param_names, counter, name_map);
        }
        _ => {}
    }
}

/// Recursively transform a mutable `Expr`, replacing placeholders in place.
pub fn transform_expr(
    expr: &mut Expr,
    param_names: &mut Vec<Option<String>>,
    counter: &mut usize,
    name_map: &mut std::collections::HashMap<String, usize>,
) {
    match expr {
        // ── Placeholder hit ─────────────────────────────────────────────
        Expr::Value(ValueWithSpan {
            value: Value::Placeholder(s),
            ..
        }) => {
            let placeholder = s.clone();
            let n = if placeholder == "?" {
                // unnamed – always a new slot
                let n = *counter;
                *counter += 1;
                param_names.push(None);
                n
            } else if let Some(name) = placeholder.strip_prefix(':') {
                // named – reuse if seen before
                if let Some(&existing_n) = name_map.get(name) {
                    existing_n
                } else {
                    let n = *counter;
                    *counter += 1;
                    name_map.insert(name.to_string(), n);
                    param_names.push(Some(name.to_string()));
                    n
                }
            } else {
                // already a $N or unknown – leave alone
                return;
            };
            *s = format!("${n}");
        }

        // ── Recurse into all sub-expressions ────────────────────────────
        Expr::BinaryOp { left, right, .. } => {
            transform_expr(left, param_names, counter, name_map);
            transform_expr(right, param_names, counter, name_map);
        }
        Expr::UnaryOp { expr: inner, .. } => {
            transform_expr(inner, param_names, counter, name_map);
        }
        Expr::InList { expr: e, list, .. } => {
            transform_expr(e, param_names, counter, name_map);
            for item in list {
                transform_expr(item, param_names, counter, name_map);
            }
        }
        Expr::InSubquery {
            expr: e, subquery, ..
        } => {
            transform_expr(e, param_names, counter, name_map);
            transform_query(subquery, param_names, counter, name_map);
        }
        Expr::Between {
            expr: e, low, high, ..
        } => {
            transform_expr(e, param_names, counter, name_map);
            transform_expr(low, param_names, counter, name_map);
            transform_expr(high, param_names, counter, name_map);
        }
        Expr::Function(f) => {
            if let FunctionArguments::List(ref mut arg_list) = f.args {
                for arg in &mut arg_list.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            transform_expr(e, param_names, counter, name_map);
                        }
                        FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            transform_expr(e, param_names, counter, name_map);
                        }
                        _ => {}
                    }
                }
            }
        }
        Expr::Cast { expr: inner, .. } => {
            transform_expr(inner, param_names, counter, name_map);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                transform_expr(op, param_names, counter, name_map);
            }
            for cw in conditions {
                transform_expr(&mut cw.condition, param_names, counter, name_map);
                transform_expr(&mut cw.result, param_names, counter, name_map);
            }
            if let Some(er) = else_result {
                transform_expr(er, param_names, counter, name_map);
            }
        }
        Expr::Nested(inner) => {
            transform_expr(inner, param_names, counter, name_map);
        }
        Expr::Subquery(q) => {
            transform_query(q, param_names, counter, name_map);
        }
        Expr::Exists { subquery, .. } => {
            transform_query(subquery, param_names, counter, name_map);
        }
        Expr::Extract { expr: inner, .. } => {
            transform_expr(inner, param_names, counter, name_map);
        }
        Expr::Like {
            expr: e, pattern, ..
        } => {
            transform_expr(e, param_names, counter, name_map);
            transform_expr(pattern, param_names, counter, name_map);
        }
        Expr::ILike {
            expr: e, pattern, ..
        } => {
            transform_expr(e, param_names, counter, name_map);
            transform_expr(pattern, param_names, counter, name_map);
        }
        _ => {}
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    #[test]
    fn rewrite_placeholders__placeholder_question_mark() {
        let r = translate("SELECT * FROM t WHERE a = ? AND b = ?").unwrap();
        assert!(r.sql.contains("$1") && r.sql.contains("$2"));
        assert_eq!(r.param_names.len(), 2, "sql={}", r.sql);
        assert_eq!(r.param_names[0], None);
        assert_eq!(r.param_names[1], None);
    }

    #[test]
    fn rewrite_placeholders__placeholder_named() {
        let r = translate("SELECT * FROM t WHERE id = :id").unwrap();
        assert!(r.sql.contains("$1"));
        assert_eq!(r.param_names[0], Some("id".to_string()));
    }

    #[test]
    fn rewrite_placeholders__placeholder_named_multiple() {
        let r = translate("SELECT * FROM t WHERE a = :foo AND b = :bar AND c = :baz").unwrap();
        assert!(r.sql.contains("$1") && r.sql.contains("$2") && r.sql.contains("$3"));
        assert_eq!(r.param_names.len(), 3);
    }

    #[test]
    fn rewrite_placeholders__placeholder_named_reuse() {
        let r = translate("SELECT * FROM t WHERE a = :id OR b = :id").unwrap();
        // same :id reuses same $N
        assert_eq!(r.param_names.len(), 1);
        assert_eq!(r.sql.matches("$1").count(), 2);
    }

    #[test]
    fn rewrite_placeholders__placeholder_in_string_literal_ignored() {
        let r = translate("SELECT * FROM t WHERE a = ':not_a_param'").unwrap();
        assert_eq!(r.param_names.len(), 0);
        assert!(r.sql.contains(":not_a_param"));
    }

    #[test]
    fn rewrite_placeholders__placeholder_mixed_question_and_named() {
        let r = translate("SELECT * FROM t WHERE a = ? AND b = :foo AND c = ?").unwrap();
        assert_eq!(r.param_names.len(), 3);
        assert!(r.sql.contains("$1") && r.sql.contains("$2") && r.sql.contains("$3"));
    }

    #[test]
    fn rewrite_placeholders__placeholder_in_join_on_clause() {
        let r = translate("select metadata_items.id from metadata_item_settings join metadata_items on metadata_items.guid=metadata_item_settings.guid and metadata_item_settings.account_id=? where metadata_items.library_section_id=? order by metadata_item_settings.updated_at desc limit ?").unwrap();
        assert_eq!(r.param_names.len(), 3);
        assert!(r.sql.contains("account_id = $1"), "sql={}", r.sql);
        assert!(r.sql.contains("library_section_id = $2"), "sql={}", r.sql);
        assert!(r.sql.contains("LIMIT $3"), "sql={}", r.sql);
        assert!(!r.sql.contains("?"), "sql={}", r.sql);
    }

    #[test]
    fn rewrite_placeholders__placeholder_in_nested_join_on_clause() {
        let r = translate(
            "SELECT * FROM (a JOIN b ON a.id = b.a_id AND b.account_id = ?) LEFT JOIN c ON c.id = b.c_id AND c.library_id = ?",
        )
        .unwrap();
        assert_eq!(r.param_names.len(), 2, "sql={}", r.sql);
        assert!(r.sql.contains("$1"), "sql={}", r.sql);
        assert!(r.sql.contains("$2"), "sql={}", r.sql);
        assert!(!r.sql.contains("?"), "sql={}", r.sql);
    }
}
