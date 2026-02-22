/// Module: quotes
///
/// Rewrites quote styles to PostgreSQL-compatible double-quotes:
///   Backtick-quoted identifiers  → double-quote
///   CREATE TABLE                 → CREATE TABLE IF NOT EXISTS
///   CREATE INDEX                 → CREATE INDEX IF NOT EXISTS
use sqlparser::ast::*;

pub fn transform(stmt: &mut Statement) {
    match stmt {
        Statement::Query(q) => transform_query(q),

        Statement::Insert(ins) => {
            // Fix identifier quotes in column lists and source
            for col in &mut ins.columns {
                fix_ident(col);
            }
            if let Some(src) = &mut ins.source {
                transform_query(src);
            }
            // Unquote ON CONFLICT target columns (PostgreSQL wants unquoted)
            if let Some(OnInsert::OnConflict(ref mut oc)) = ins.on {
                if let Some(ConflictTarget::Columns(ref mut cols)) = oc.conflict_target {
                    for col in cols {
                        col.quote_style = None;
                    }
                }
            }
        }

        Statement::Update(u) => {
            for assign in &mut u.assignments {
                // Fix backtick-quoted column names on the left-hand side
                match &mut assign.target {
                    AssignmentTarget::ColumnName(name) => fix_object_name(name),
                    AssignmentTarget::Tuple(names) => {
                        for name in names {
                            fix_object_name(name);
                        }
                    }
                }
                fix_expr(&mut assign.value);
            }
            if let Some(sel) = &mut u.selection {
                fix_expr(sel);
            }
        }

        Statement::Delete(d) => {
            if let Some(sel) = &mut d.selection {
                fix_expr(sel);
            }
        }

        Statement::CreateTable(ct) => {
            // Set IF NOT EXISTS
            ct.if_not_exists = true;
            // Fix table name quotes (single-quote → double-quote)
            fix_object_name(&mut ct.name);
            // Fix column names and types
            for col in &mut ct.columns {
                fix_ident(&mut col.name);
            }
        }

        Statement::CreateIndex(ci) => {
            // Set IF NOT EXISTS
            ci.if_not_exists = true;
        }

        Statement::AlterTable(at) => {
            // Fix table name quotes
            fix_object_name(&mut at.name);
            // ALTER TABLE ADD → ADD COLUMN IF NOT EXISTS
            for op in &mut at.operations {
                if let AlterTableOperation::AddColumn {
                    ref mut column_keyword,
                    ref mut if_not_exists,
                    ref mut column_def,
                    ..
                } = op
                {
                    *column_keyword = true;
                    *if_not_exists = true;
                    fix_ident(&mut column_def.name);
                }
            }
        }

        _ => {}
    }
}

// ─── Query traversal ──────────────────────────────────────────────────────────

fn transform_query(q: &mut Query) {
    if let Some(with) = &mut q.with {
        for cte in &mut with.cte_tables {
            transform_query(&mut cte.query);
        }
    }
    transform_set_expr(&mut q.body);
    if let Some(ob) = &mut q.order_by {
        if let OrderByKind::Expressions(exprs) = &mut ob.kind {
            for oe in exprs {
                fix_expr(&mut oe.expr);
            }
        }
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
        SetExpr::Values(vals) => {
            for row in &mut vals.rows {
                for e in row {
                    fix_expr(e);
                }
            }
        }
        _ => {}
    }
}

fn transform_select(sel: &mut Select) {
    for item in &mut sel.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                fix_expr(e);
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                if let SelectItemQualifiedWildcardKind::ObjectName(ref mut name) = kind {
                    fix_object_name(name);
                }
            }
            _ => {}
        }
        // Also fix alias quote style
        if let SelectItem::ExprWithAlias { alias, .. } = item {
            fix_ident(alias);
        }
    }

    for twj in &mut sel.from {
        transform_table_with_joins(twj);
    }

    if let Some(w) = &mut sel.selection {
        fix_expr(w);
    }

    if let GroupByExpr::Expressions(exprs, _) = &mut sel.group_by {
        for e in exprs {
            fix_expr(e);
        }
    }

    if let Some(h) = &mut sel.having {
        fix_expr(h);
    }
}

fn transform_table_with_joins(twj: &mut TableWithJoins) {
    transform_table_factor(&mut twj.relation);
    for join in &mut twj.joins {
        transform_table_factor(&mut join.relation);
        match &mut join.join_operator {
            JoinOperator::Join(JoinConstraint::On(e))
            | JoinOperator::Inner(JoinConstraint::On(e))
            | JoinOperator::LeftOuter(JoinConstraint::On(e))
            | JoinOperator::RightOuter(JoinConstraint::On(e))
            | JoinOperator::FullOuter(JoinConstraint::On(e)) => {
                fix_expr(e);
            }
            _ => {}
        }
    }
}

fn transform_table_factor(tf: &mut TableFactor) {
    match tf {
        TableFactor::Table { name, alias, .. } => {
            fix_object_name(name);
            if let Some(a) = alias {
                fix_ident(&mut a.name);
            }
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => {
            transform_query(subquery);
            if let Some(a) = alias {
                fix_ident(&mut a.name);
            }
        }
        _ => {}
    }
}

// ─── Identifier fixers ────────────────────────────────────────────────────────

/// Fix a single `Ident`: backtick → double-quote,
/// and add double-quotes to unquoted identifiers that contain uppercase letters
/// (PostgreSQL folds unquoted identifiers to lowercase, so mixed-case names
/// must be quoted to preserve their case).
fn fix_ident(ident: &mut Ident) {
    if ident.quote_style == Some('`') || ident.quote_style == Some('\'') {
        // Backtick and single-quote identifiers → double-quote
        ident.quote_style = Some('"');
    } else if ident.quote_style.is_none() && needs_quoting(&ident.value) {
        ident.quote_style = Some('"');
    }
}

/// Fix a single `Ident` in a function-name context: only backtick → double-quote.
/// We must NOT add quotes to function names like COALESCE, COUNT, etc. because
/// PostgreSQL stores function names in lowercase and `"COALESCE"` would fail.
fn fix_ident_func(ident: &mut Ident) {
    if ident.quote_style == Some('`') {
        ident.quote_style = Some('"');
    }
}

/// Returns true if an unquoted identifier needs double-quoting for PostgreSQL.
/// Any identifier containing ASCII uppercase letters needs quoting because
/// PostgreSQL folds unquoted identifiers to lowercase.
fn needs_quoting(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.chars().any(|c| c.is_ascii_uppercase())
}

/// Fix all identifiers in an `ObjectName`.
fn fix_object_name(name: &mut ObjectName) {
    for part in &mut name.0 {
        if let ObjectNamePart::Identifier(ref mut i) = part {
            fix_ident(i);
        }
    }
}

/// Fix all identifiers in a function `ObjectName` — backtick-only, no mixed-case quoting.
fn fix_object_name_func(name: &mut ObjectName) {
    for part in &mut name.0 {
        if let ObjectNamePart::Identifier(ref mut i) = part {
            fix_ident_func(i);
        }
    }
}

/// Recursively fix all identifiers in an expression.
fn fix_expr(expr: &mut Expr) {
    match expr {
        Expr::Identifier(i) => fix_ident(i),
        Expr::CompoundIdentifier(parts) => {
            for i in parts {
                fix_ident(i);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            fix_expr(left);
            fix_expr(right);
        }
        Expr::UnaryOp { expr: inner, .. } => fix_expr(inner),
        Expr::Nested(inner) => fix_expr(inner),
        Expr::Cast { expr: inner, .. } => fix_expr(inner),
        Expr::InList { expr, list, .. } => {
            fix_expr(expr);
            for e in list {
                fix_expr(e);
            }
        }
        Expr::InSubquery { expr, subquery, .. } => {
            fix_expr(expr);
            transform_query(subquery);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            fix_expr(expr);
            fix_expr(low);
            fix_expr(high);
        }
        Expr::Function(f) => {
            fix_object_name_func(&mut f.name);
            if let FunctionArguments::List(ref mut al) = f.args {
                for arg in &mut al.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => fix_expr(e),
                        FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(e),
                            ..
                        } => fix_expr(e),
                        _ => {}
                    }
                }
            }
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                fix_expr(op);
            }
            for cw in conditions {
                fix_expr(&mut cw.condition);
                fix_expr(&mut cw.result);
            }
            if let Some(er) = else_result {
                fix_expr(er);
            }
        }
        Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => {
            transform_query(q);
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            fix_expr(expr);
            fix_expr(pattern);
        }
        _ => {}
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::translate;

    #[test]
    fn quotes_backtick_to_double_quote() {
        let r = translate("SELECT `id`, `title` FROM `metadata_items`").unwrap();
        assert!(!r.sql.contains('`'));
        assert!(r.sql.contains('"'));
    }

    #[test]
    fn quotes_double_quotes_preserved() {
        let r = translate(r#"SELECT "id" FROM "table""#).unwrap();
        assert!(r.sql.contains('"'));
    }

    #[test]
    fn quotes_create_table_if_not_exists_added() {
        let r = translate("CREATE TABLE foo (id INTEGER)").unwrap();
        assert!(r.sql.to_uppercase().contains("IF NOT EXISTS"));
    }

    #[test]
    fn quotes_create_table_if_not_exists_not_doubled() {
        let r = translate("CREATE TABLE IF NOT EXISTS foo (id INTEGER PRIMARY KEY)").unwrap();
        let count = r.sql.to_uppercase().matches("IF NOT EXISTS").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn quotes_create_index_if_not_exists_added() {
        let r = translate("CREATE INDEX idx_foo ON t(id)").unwrap();
        assert!(r.sql.to_uppercase().contains("IF NOT EXISTS"));
    }

    #[test]
    fn quotes_join_on_mixed_case() {
        let r = translate("select taggings.id as blankKeyTaggingId, otherTags.id as nonblankKeyId from tags join tags as otherTags on otherTags.tag = tags.tag where tags.tag_value = ?").unwrap();
        assert!(
            r.sql.contains("\"otherTags\".tag"),
            "otherTags ref in ON clause should be quoted, got: {}",
            r.sql
        );
    }
}
