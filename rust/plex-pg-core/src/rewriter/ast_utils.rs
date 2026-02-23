use sqlparser::ast::{CastKind, DataType, Expr, Value, ValueWithSpan};
use sqlparser::tokenizer::Span;

pub fn null_expr() -> Expr {
    Expr::Value(ValueWithSpan {
        value: Value::Null,
        span: Span::empty(),
    })
}

pub fn take_expr(expr: &mut Expr) -> Expr {
    std::mem::replace(expr, null_expr())
}

pub fn take_boxed_expr(expr: &mut Box<Expr>) -> Expr {
    take_expr(expr.as_mut())
}

pub fn wrap_double_colon_cast(expr: Expr, data_type: DataType) -> Expr {
    Expr::Cast {
        kind: CastKind::DoubleColon,
        expr: Box::new(expr),
        data_type,
        array: false,
        format: None,
    }
}
