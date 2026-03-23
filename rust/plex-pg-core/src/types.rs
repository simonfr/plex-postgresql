/// Module: types
///
/// Rewrites SQLite DDL types to PostgreSQL equivalents in CREATE TABLE:
///   BLOB                → BYTEA
///   datetime            → TIMESTAMP
///   integer(8) / INTEGER(8) → BIGINT  (also dt_integer)
///   AUTOINCREMENT       → removed (handled via ColumnOption::DialectSpecific)
///   DEFAULT 't'         → DEFAULT TRUE
///   DEFAULT 'f'         → DEFAULT FALSE
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;

pub fn transform(stmt: &mut Statement) {
    if let Statement::CreateTable(ct) = stmt {
        for col in &mut ct.columns {
            transform_column(col);
        }
    }
}

fn transform_column(col: &mut ColumnDef) {
    // 1. Rewrite the data type
    col.data_type = rewrite_data_type(std::mem::replace(&mut col.data_type, DataType::Unspecified));

    // 2. Check for AUTOINCREMENT and remove it
    let had_autoincrement = col.options.iter().any(|opt_def| {
        if let ColumnOption::DialectSpecific(tokens) = &opt_def.option {
            let text: String = tokens
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            text.to_uppercase().contains("AUTOINCREMENT")
        } else {
            false
        }
    });

    col.options.retain(|opt_def| match &opt_def.option {
        ColumnOption::DialectSpecific(tokens) => {
            let text: String = tokens
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            !text.to_uppercase().contains("AUTOINCREMENT")
        }
        _ => true,
    });

    // 2b. If AUTOINCREMENT was present, change INTEGER → SERIAL (or BIGINT → BIGSERIAL)
    if had_autoincrement {
        col.data_type = match &col.data_type {
            DataType::Integer(_) | DataType::Int(_) => DataType::Custom(
                ObjectName(vec![ObjectNamePart::Identifier(Ident::new("SERIAL"))]),
                vec![],
            ),
            DataType::BigInt(_) => DataType::Custom(
                ObjectName(vec![ObjectNamePart::Identifier(Ident::new("BIGSERIAL"))]),
                vec![],
            ),
            _ => DataType::Custom(
                ObjectName(vec![ObjectNamePart::Identifier(Ident::new("SERIAL"))]),
                vec![],
            ),
        };
    }

    // 3. Rewrite DEFAULT 't' → TRUE, DEFAULT 'f' → FALSE
    for opt_def in &mut col.options {
        if let ColumnOption::Default(ref mut expr) = opt_def.option {
            rewrite_default_bool(expr);
        }
        if let ColumnOption::Generated {
            generation_expr_mode: ref mut mode,
            ..
        } = opt_def.option
        {
            // SQLite supports VIRTUAL generated columns; PostgreSQL expects STORED.
            if matches!(mode, Some(GeneratedExpressionMode::Virtual)) {
                *mode = Some(GeneratedExpressionMode::Stored);
            }
        }
    }
}

/// Rewrite boolean string defaults.
fn rewrite_default_bool(expr: &mut Expr) {
    if let Expr::Value(ValueWithSpan { ref value, .. }) = expr {
        let replacement = match value {
            Value::SingleQuotedString(s) if s == "t" => Some(Value::Boolean(true)),
            Value::SingleQuotedString(s) if s == "f" => Some(Value::Boolean(false)),
            _ => None,
        };
        if let Some(new_val) = replacement {
            *expr = Expr::Value(ValueWithSpan {
                value: new_val,
                span: Span::empty(),
            });
        }
    }
}

/// Map SQLite data types to their PostgreSQL equivalents.
fn rewrite_data_type(dt: DataType) -> DataType {
    match dt {
        // BLOB → BYTEA
        DataType::Blob(_) => DataType::Bytea,

        // datetime (natively parsed as Datetime) → TIMESTAMP
        DataType::Datetime(_) => DataType::Timestamp(None, TimezoneInfo::None),

        // Custom types: datetime (fallback), INTEGER(8), dt_integer
        DataType::Custom(ref name, ref params) => {
            let name_str = name.to_string().to_lowercase();
            match name_str.as_str() {
                "datetime" => DataType::Timestamp(None, TimezoneInfo::None),
                "integer" | "dt_integer" => {
                    // If parameterized with 8, treat as BIGINT
                    if params.contains(&"8".to_string()) {
                        DataType::BigInt(None)
                    } else {
                        DataType::Integer(None)
                    }
                }
                _ => dt,
            }
        }

        // INT(8) / INTEGER(8) variants already parsed natively
        DataType::Int(Some(8)) | DataType::Integer(Some(8)) => DataType::BigInt(None),

        _ => dt,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    #[test]
    fn subset_core__type_blob_to_bytea() {
        let r = translate("CREATE TABLE t (data BLOB)").unwrap();
        assert!(r.sql.to_uppercase().contains("BYTEA"));
        assert!(!r.sql.to_uppercase().contains("BLOB"));
    }

    #[test]
    fn subset_core__type_datetime_to_timestamp() {
        let r = translate("CREATE TABLE t (created_at datetime)").unwrap();
        assert!(r.sql.to_uppercase().contains("TIMESTAMP"));
        assert!(!r.sql.to_lowercase().contains("datetime"));
    }

    #[test]
    fn subset_core__type_text_preserved() {
        let r = translate("CREATE TABLE t (name TEXT)").unwrap();
        assert!(r.sql.to_uppercase().contains("TEXT"));
    }

    #[test]
    fn subset_core__type_no_match_unchanged() {
        let r = translate("SELECT id, name FROM metadata_items").unwrap();
        assert!(r.sql.contains("metadata_items"));
    }

    #[test]
    fn subset_core__type_default_t_to_true() {
        let r = translate("CREATE TABLE t (active INTEGER DEFAULT 't')").unwrap();
        assert!(r.sql.to_uppercase().contains("TRUE"));
        assert!(!r.sql.contains("'t'"));
    }

    #[test]
    fn subset_core__type_default_f_to_false() {
        let r = translate("CREATE TABLE t (deleted INTEGER DEFAULT 'f')").unwrap();
        assert!(r.sql.to_uppercase().contains("FALSE"));
        assert!(!r.sql.contains("'f'"));
    }
}
