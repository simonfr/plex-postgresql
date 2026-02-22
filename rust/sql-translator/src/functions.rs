use sqlparser::ast::helpers::attached_token::AttachedToken;
/// Module: functions
///
/// Rewrites SQLite function calls to their PostgreSQL equivalents:
///   IFNULL(a,b)           → COALESCE(a,b)
///   iif(cond,then,else)   → CASE WHEN cond THEN then ELSE else END
///   typeof(x)             → pg_typeof(x)::text
///   last_insert_rowid()   → lastval()
///   json_each(x)          → json_array_elements(x)
///   SUBSTR(a,b,c)         → SUBSTRING(a,b,c)
///   instr(a,b)            → STRPOS(a,b)   (same arg order)
///   strftime('%s',…)      → EXTRACT(EPOCH FROM …)::bigint
///   strftime('%Y-%m-%d',…)→ TO_CHAR(…, 'YYYY-MM-DD')
///   unixepoch(…)          → EXTRACT(EPOCH FROM …)::bigint
///   datetime('now')       → NOW()
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;

pub fn transform(stmt: &mut Statement) {
    transform_stmt(stmt);
}

fn transform_stmt(stmt: &mut Statement) {
    match stmt {
        Statement::Query(q) => transform_query(q),
        Statement::Insert(ins) => {
            if let Some(src) = &mut ins.source {
                transform_query(src);
            }
        }
        Statement::Update(update) => {
            for assign in &mut update.assignments {
                transform_expr_inplace(&mut assign.value);
            }
            if let Some(sel) = &mut update.selection {
                transform_expr_inplace(sel);
            }
        }
        Statement::Delete(del) => {
            if let Some(sel) = &mut del.selection {
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
    transform_set_expr(&mut q.body);
    if let Some(ob) = &mut q.order_by {
        if let OrderByKind::Expressions(exprs) = &mut ob.kind {
            for oe in exprs {
                transform_expr_inplace(&mut oe.expr);
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
                    transform_expr_inplace(e);
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
                transform_expr_inplace(e);
            }
            _ => {}
        }
    }
    for table in &mut sel.from {
        transform_table_with_joins(table);
    }

    // After FROM transformation, check if json_array_elements is present.
    // If so, cast bare `value` references in projection to ::text.
    if has_json_array_elements_in_from(&sel.from) {
        for item in &mut sel.projection {
            match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    cast_json_value_to_text(e);
                }
                _ => {}
            }
        }
    }

    if let Some(sel_where) = &mut sel.selection {
        transform_expr_inplace(sel_where);
    }
    match &mut sel.group_by {
        GroupByExpr::Expressions(exprs, _) => {
            for e in exprs {
                transform_expr_inplace(e);
            }
        }
        _ => {}
    }
    if let Some(having) = &mut sel.having {
        transform_expr_inplace(having);
    }
}

/// Check if any FROM table is json_array_elements (was json_each before transformation)
fn has_json_array_elements_in_from(from: &[TableWithJoins]) -> bool {
    for twj in from {
        if is_json_array_elements_table(&twj.relation) {
            return true;
        }
        for join in &twj.joins {
            if is_json_array_elements_table(&join.relation) {
                return true;
            }
        }
    }
    false
}

fn is_json_array_elements_table(tf: &TableFactor) -> bool {
    if let TableFactor::Table { name, .. } = tf {
        let table_name = name
            .0
            .last()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .unwrap_or_default();
        return table_name == "json_array_elements";
    }
    false
}

/// Cast bare `value` identifiers to ::text (for json_array_elements output)
fn cast_json_value_to_text(expr: &mut Expr) {
    if let Expr::Identifier(ref ident) = expr {
        if ident.value.to_lowercase() == "value" {
            let inner = std::mem::replace(
                expr,
                Expr::Value(ValueWithSpan {
                    value: Value::Null,
                    span: Span::empty(),
                }),
            );
            *expr = Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(inner),
                data_type: DataType::Text,
                array: false,
                format: None,
            };
        }
    }
}

fn transform_table_with_joins(t: &mut TableWithJoins) {
    transform_table_factor(&mut t.relation);
    for join in &mut t.joins {
        transform_table_factor(&mut join.relation);
        match &mut join.join_operator {
            JoinOperator::Join(JoinConstraint::On(e))
            | JoinOperator::Inner(JoinConstraint::On(e))
            | JoinOperator::LeftOuter(JoinConstraint::On(e))
            | JoinOperator::RightOuter(JoinConstraint::On(e))
            | JoinOperator::FullOuter(JoinConstraint::On(e)) => {
                transform_expr_inplace(e);
            }
            _ => {}
        }
    }
}

fn transform_table_factor(f: &mut TableFactor) {
    match f {
        TableFactor::Derived { subquery, .. } => {
            transform_query(subquery);
        }
        TableFactor::Table { name, args, .. } => {
            // json_each(x) in FROM clause → json_array_elements(x::json)
            let table_name = name
                .0
                .last()
                .and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
                .unwrap_or_default();
            if table_name == "json_each" {
                // Rename to json_array_elements
                if let Some(last) = name.0.last_mut() {
                    if let ObjectNamePart::Identifier(ref mut i) = last {
                        i.value = "json_array_elements".to_string();
                    }
                }
                // Add ::json cast to the argument
                if let Some(TableFunctionArgs { ref mut args, .. }) = args {
                    for arg in args.iter_mut() {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(ref mut e)) = arg {
                            *e = Expr::Cast {
                                kind: CastKind::DoubleColon,
                                expr: Box::new(std::mem::replace(
                                    e,
                                    Expr::Value(ValueWithSpan {
                                        value: Value::Null,
                                        span: Span::empty(),
                                    }),
                                )),
                                data_type: DataType::JSON,
                                array: false,
                                format: None,
                            };
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// The main recursive transformer.  Returns the expression that should
/// replace the input (usually `None` → keep self, but for `iif` we
/// swap the whole node).
fn transform_expr(expr: Expr) -> Expr {
    match expr {
        Expr::Function(mut func) => {
            // Get the lowercase function name for matching
            let fn_name = func
                .name
                .0
                .first()
                .and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
                .unwrap_or_default();

            // simplify_typeof_fixup: iif(typeof(X) in ('integer','real'), X, ...) → X
            // Must run BEFORE iif→CASE conversion
            if fn_name == "iif" {
                if let Some(simplified) = try_simplify_typeof(&func) {
                    return transform_expr(simplified);
                }
            }

            // First recurse into the args
            if let FunctionArguments::List(ref mut arg_list) = func.args {
                for arg in &mut arg_list.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            *e = transform_expr(std::mem::replace(
                                e,
                                Expr::Value(ValueWithSpan {
                                    value: Value::Null,
                                    span: sqlparser::tokenizer::Span::empty(),
                                }),
                            ));
                        }
                        FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            *e = transform_expr(std::mem::replace(
                                e,
                                Expr::Value(ValueWithSpan {
                                    value: Value::Null,
                                    span: sqlparser::tokenizer::Span::empty(),
                                }),
                            ));
                        }
                        _ => {}
                    }
                }
            }

            match fn_name.as_str() {
                // IFNULL → COALESCE
                "ifnull" => {
                    rename_function(&mut func, "COALESCE");
                    Expr::Function(func)
                }

                // iif(cond, then, else) → CASE WHEN cond THEN then ELSE else END
                "iif" => {
                    let args = extract_unnamed_args(&mut func);
                    if args.len() >= 3 {
                        let mut iter = args.into_iter();
                        let cond = iter.next().unwrap();
                        let then = iter.next().unwrap();
                        let else_e = iter.next().unwrap();
                        Expr::Case {
                            case_token: AttachedToken::empty(),
                            end_token: AttachedToken::empty(),
                            operand: None,
                            conditions: vec![CaseWhen {
                                condition: cond,
                                result: then,
                            }],
                            else_result: Some(Box::new(else_e)),
                        }
                    } else {
                        Expr::Function(func)
                    }
                }

                // typeof(x) → pg_typeof(x)::text
                "typeof" => {
                    rename_function(&mut func, "pg_typeof");
                    Expr::Cast {
                        kind: CastKind::DoubleColon,
                        expr: Box::new(Expr::Function(func)),
                        data_type: DataType::Text,
                        array: false,
                        format: None,
                    }
                }

                // last_insert_rowid() → lastval()
                "last_insert_rowid" => {
                    rename_function(&mut func, "lastval");
                    Expr::Function(func)
                }

                // json_each(x) → json_array_elements(x)
                "json_each" => {
                    rename_function(&mut func, "json_array_elements");
                    Expr::Function(func)
                }

                // SUBSTR → SUBSTRING
                "substr" => {
                    rename_function(&mut func, "SUBSTRING");
                    Expr::Function(func)
                }

                // instr(haystack, needle) → STRPOS(haystack, needle)  (same arg order)
                "instr" => {
                    rename_function(&mut func, "STRPOS");
                    Expr::Function(func)
                }

                // strftime('%s', expr) → EXTRACT(EPOCH FROM expr)::bigint
                // strftime('%Y-%m-%d', expr) → TO_CHAR(expr, 'YYYY-MM-DD')
                "strftime" => transform_strftime(func),

                // unixepoch(expr) → EXTRACT(EPOCH FROM expr)::bigint
                // unixepoch('now', '-7 day') → EXTRACT(EPOCH FROM NOW() - INTERVAL '7 day')::bigint
                "unixepoch" => {
                    let args = extract_unnamed_args(&mut func);
                    let mut iter = args.into_iter();
                    let inner_expr = iter.next().unwrap_or_else(|| make_now_call());
                    let mut source = if is_now_literal(&inner_expr) {
                        make_now_call()
                    } else {
                        inner_expr
                    };
                    if let Some(interval_expr) = iter.next() {
                        source = apply_interval(source, interval_expr);
                    }
                    Expr::Cast {
                        kind: CastKind::DoubleColon,
                        expr: Box::new(Expr::Extract {
                            field: DateTimeField::Epoch,
                            syntax: ExtractSyntax::From,
                            expr: Box::new(source),
                        }),
                        data_type: DataType::BigInt(None),
                        array: false,
                        format: None,
                    }
                }

                // datetime('now') → NOW()
                "datetime" => {
                    let args = extract_unnamed_args(&mut func);
                    let inner = args.into_iter().next().unwrap_or_else(|| make_now_call());
                    if is_now_literal(&inner) {
                        make_now_call()
                    } else {
                        inner
                    }
                }

                _ => Expr::Function(func),
            }
        }

        // Recurse into other expression forms
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
        Expr::Case {
            case_token,
            end_token,
            operand,
            conditions,
            else_result,
        } => {
            let operand = operand.map(|o| Box::new(transform_expr(*o)));
            let conditions = conditions
                .into_iter()
                .map(|cw| CaseWhen {
                    condition: transform_expr(cw.condition),
                    result: transform_expr(cw.result),
                })
                .collect();
            let else_result = else_result.map(|e| Box::new(transform_expr(*e)));
            Expr::Case {
                case_token,
                end_token,
                operand,
                conditions,
                else_result,
            }
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let transformed_expr = transform_expr(*expr);
            let mut transformed_list: Vec<Expr> = list.into_iter().map(transform_expr).collect();
            // typeof type remapping: when expr is pg_typeof(...)::text,
            // add PostgreSQL type aliases to the IN list
            if is_pg_typeof_cast(&transformed_expr) {
                add_typeof_remappings(&mut transformed_list);
            }
            Expr::InList {
                expr: Box::new(transformed_expr),
                list: transformed_list,
                negated,
            }
        }
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
        // SUBSTR(a,b,c) is parsed as Expr::Substring { shorthand: true }
        // We convert it to SUBSTRING by clearing the shorthand flag
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            special,
            shorthand: true,
        } => Expr::Substring {
            expr: Box::new(transform_expr(*expr)),
            substring_from: substring_from.map(|e| Box::new(transform_expr(*e))),
            substring_for: substring_for.map(|e| Box::new(transform_expr(*e))),
            special,
            shorthand: false, // force SUBSTRING output
        },
        other => other,
    }
}

/// In-place wrapper for `transform_expr`.
fn transform_expr_inplace(expr: &mut Expr) {
    let taken = std::mem::replace(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::Null,
            span: sqlparser::tokenizer::Span::empty(),
        }),
    );
    *expr = transform_expr(taken);
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn rename_function(func: &mut Function, new_name: &str) {
    func.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(new_name))]);
}

/// Check if an expression is pg_typeof(...)::text (the result of typeof → pg_typeof conversion)
fn is_pg_typeof_cast(expr: &Expr) -> bool {
    if let Expr::Cast {
        expr: inner,
        data_type: DataType::Text,
        ..
    } = expr
    {
        if let Expr::Function(func) = inner.as_ref() {
            let name = func
                .name
                .0
                .first()
                .and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
                .unwrap_or_default();
            return name == "pg_typeof";
        }
    }
    false
}

/// Remap PostgreSQL type names in an IN list for pg_typeof comparisons.
/// SQLite typeof returns 'integer', 'real', etc. PostgreSQL pg_typeof returns
/// 'bigint', 'double precision', etc.
/// - 'integer' → keep AND add 'bigint'
/// - 'real' → REPLACE with 'double precision'
fn add_typeof_remappings(list: &mut Vec<Expr>) {
    let mut additions = Vec::new();
    for item in list.iter_mut() {
        if let Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(ref mut s),
            ..
        }) = item
        {
            match s.to_lowercase().as_str() {
                "integer" => {
                    additions.push(make_string_literal("bigint"));
                }
                "real" => {
                    // Replace 'real' with 'double precision' (not alongside)
                    *s = "double precision".to_string();
                }
                _ => {}
            }
        }
    }
    list.extend(additions);
}

fn make_string_literal(s: &str) -> Expr {
    Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(s.to_string()),
        span: Span::empty(),
    })
}

/// Detect the pattern: iif(typeof(X) in ('integer','real'), X, strftime('%s', X, 'utc'))
/// and simplify to just X. In PostgreSQL, columns have fixed types so the
/// conditional typeof check is unnecessary.
fn try_simplify_typeof(func: &Function) -> Option<Expr> {
    if let FunctionArguments::List(ref al) = func.args {
        if al.args.len() >= 2 {
            // First arg should be: typeof(X) IN ('integer', 'real', ...)
            let first = match &al.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => e,
                _ => return None,
            };
            // Check if first arg is typeof(...) IN (...)
            if let Expr::InList { expr, .. } = first {
                if let Expr::Function(inner_func) = expr.as_ref() {
                    let inner_name = inner_func
                        .name
                        .0
                        .first()
                        .and_then(|p| match p {
                            ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    if inner_name == "typeof" {
                        // Second arg is the "then" value — just return it
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(then_expr)) = &al.args[1]
                        {
                            return Some(then_expr.clone());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Extract all `FunctionArg::Unnamed(FunctionArgExpr::Expr(…))` args.
fn extract_unnamed_args(func: &mut Function) -> Vec<Expr> {
    let mut result = Vec::new();
    if let FunctionArguments::List(ref mut al) = func.args {
        for arg in al.args.drain(..) {
            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                result.push(e);
            }
        }
    }
    result
}

fn is_now_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(s), .. }) if s.to_lowercase() == "now")
}

fn make_now_call() -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("NOW"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

/// Apply a SQLite interval modifier (e.g. `'-7 day'`) to a source expression.
/// Produces `source + INTERVAL '...'` or `source - INTERVAL '...'` depending on sign.
fn apply_interval(source: Expr, interval_expr: Expr) -> Expr {
    let interval_str = match &interval_expr {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            ..
        }) => s.clone(),
        _ => return source,
    };

    let (op, cleaned) = if let Some(rest) = interval_str.strip_prefix('-') {
        (BinaryOperator::Minus, rest.trim().to_string())
    } else if let Some(rest) = interval_str.strip_prefix('+') {
        (BinaryOperator::Plus, rest.trim().to_string())
    } else {
        (BinaryOperator::Plus, interval_str.trim().to_string())
    };

    let interval = Expr::Interval(Interval {
        value: Box::new(Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(cleaned),
            span: Span::empty(),
        })),
        leading_field: None,
        leading_precision: None,
        last_field: None,
        fractional_seconds_precision: None,
    });

    Expr::BinaryOp {
        left: Box::new(source),
        op,
        right: Box::new(interval),
    }
}

fn transform_strftime(mut func: Function) -> Expr {
    let args = extract_unnamed_args(&mut func);
    if args.len() < 2 {
        return Expr::Function(func);
    }
    let mut iter = args.into_iter();
    let fmt_arg = iter.next().unwrap();
    let time_arg = iter.next().unwrap();
    let interval_arg = iter.next();

    let fmt_str = match &fmt_arg {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            ..
        }) => s.clone(),
        _ => return Expr::Function(func),
    };

    let is_now = is_now_literal(&time_arg);
    let mut source = if is_now { make_now_call() } else { time_arg };

    // Apply interval if 3rd arg present
    if let Some(interval_expr) = interval_arg {
        source = apply_interval(source, interval_expr);
    }

    match fmt_str.as_str() {
        "%s" => {
            // For non-NOW column sources, wrap in TO_TIMESTAMP() first
            // because the column likely stores a Unix timestamp integer
            if !is_now {
                source = make_to_timestamp(source);
            }
            // → EXTRACT(EPOCH FROM source)::bigint
            Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(Expr::Extract {
                    field: DateTimeField::Epoch,
                    syntax: ExtractSyntax::From,
                    expr: Box::new(source),
                }),
                data_type: DataType::BigInt(None),
                array: false,
                format: None,
            }
        }
        _ => {
            // Translate strftime format to PostgreSQL TO_CHAR format
            let pg_fmt_str = translate_strftime_format(&fmt_str);
            make_to_char(source, &pg_fmt_str)
        }
    }
}

/// Translate a SQLite strftime format string to a PostgreSQL TO_CHAR format string.
fn translate_strftime_format(fmt: &str) -> String {
    let mut result = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&spec) = chars.peek() {
                chars.next();
                match spec {
                    'Y' => result.push_str("YYYY"),
                    'm' => result.push_str("MM"),
                    'd' => result.push_str("DD"),
                    'H' => result.push_str("HH24"),
                    'M' => result.push_str("MI"),
                    'S' => result.push_str("SS"),
                    '%' => result.push('%'),
                    other => {
                        result.push('%');
                        result.push(other);
                    }
                }
            } else {
                result.push('%');
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Build a TO_TIMESTAMP(expr) function call.
fn make_to_timestamp(source: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("TO_TIMESTAMP"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(source))],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

/// Build a TO_CHAR(expr, 'fmt') function call.
fn make_to_char(source: Expr, pg_fmt: &str) -> Expr {
    let fmt_expr = Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(pg_fmt.to_string()),
        span: sqlparser::tokenizer::Span::empty(),
    });
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("TO_CHAR"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(source)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(fmt_expr)),
            ],
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
    fn function_ifnull_to_coalesce() {
        let r = translate("SELECT IFNULL(a, 0) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("COALESCE"));
        assert!(!r.sql.to_uppercase().contains("IFNULL"));
    }

    #[test]
    fn function_iif_to_case() {
        let r = translate("SELECT iif(a > 0, 'yes', 'no') FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("CASE WHEN"));
        assert!(r.sql.to_uppercase().contains("THEN"));
        assert!(r.sql.to_uppercase().contains("ELSE"));
        assert!(!r.sql.to_lowercase().contains("iif"));
    }

    #[test]
    fn function_typeof_to_pg_typeof() {
        let r = translate("SELECT typeof(x) FROM t").unwrap();
        assert!(r.sql.contains("pg_typeof") || r.sql.contains("PG_TYPEOF"));
    }

    #[test]
    fn function_last_insert_rowid_to_lastval() {
        let r = translate("SELECT last_insert_rowid()").unwrap();
        assert!(r.sql.to_lowercase().contains("lastval()"));
        assert!(!r.sql.to_lowercase().contains("last_insert_rowid"));
    }

    #[test]
    fn function_substr_to_substring() {
        let r = translate("SELECT SUBSTR(a, 1, 5) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("SUBSTRING"));
    }

    #[test]
    fn function_length_preserved() {
        let r = translate("SELECT LENGTH(name) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("LENGTH"));
    }

    #[test]
    fn function_instr_to_strpos() {
        let r = translate("SELECT instr(haystack, needle) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("STRPOS"));
        assert!(!r.sql.to_lowercase().contains("instr"));
    }

    #[test]
    fn function_strftime_epoch() {
        let r = translate("SELECT strftime('%s', 'now') FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("EXTRACT"));
        assert!(r.sql.to_uppercase().contains("EPOCH"));
    }

    #[test]
    fn function_unixepoch_now() {
        let r = translate("SELECT unixepoch('now') FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("EXTRACT"));
        assert!(r.sql.to_uppercase().contains("EPOCH"));
    }

    #[test]
    fn debug_parse() {
        use sqlparser::dialect::SQLiteDialect;
        use sqlparser::parser::Parser;

        let sql1 = "SELECT strftime('%Y-%m-%d %H:%M:%S', created_at) FROM t";
        let stmts = Parser::parse_sql(&SQLiteDialect {}, sql1);
        eprintln!("Parse1: {:?}", stmts);

        let sql2 = "SELECT strftime('%s', 'now', '-7 day')";
        let stmts2 = Parser::parse_sql(&SQLiteDialect {}, sql2);
        eprintln!("Parse2: {:?}", stmts2);

        let sql3 = "SELECT datetime('now') FROM t";
        let stmts3 = Parser::parse_sql(&SQLiteDialect {}, sql3);
        eprintln!("Parse3: {:?}", stmts3);
    }
}
