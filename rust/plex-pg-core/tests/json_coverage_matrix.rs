use plex_pg_core::translate;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

fn t(sql: &str) -> String {
    translate(sql)
        .unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sql, e))
        .sql
}

fn assert_pg(sql: &str) {
    Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .unwrap_or_else(|e| panic!("postgres parse failed for `{}`: {}", sql, e));
}

#[test]
fn json_extract_simple_path() {
    let out = t("SELECT json_extract(extra_data, '$.pv:version') FROM t");
    let low = out.to_lowercase();
    assert!(low.contains("jsonb_extract_path_text"), "{}", out);
    assert!(low.contains("'pv:version'"), "{}", out);
    assert_pg(&out);
}

#[test]
fn json_extract_nested_path_with_index() {
    let out = t("SELECT json_extract(extra_data, '$.a[0].b') FROM t");
    let low = out.to_lowercase();
    assert!(low.contains("jsonb_extract_path_text"), "{}", out);
    assert!(low.contains("'a'") && low.contains("'0'") && low.contains("'b'"), "{}", out);
    assert_pg(&out);
}

#[test]
fn json_type_with_path() {
    let out = t("SELECT json_type(extra_data, '$.status') FROM t");
    let low = out.to_lowercase();
    assert!(low.contains("jsonb_typeof"), "{}", out);
    assert_pg(&out);
}

#[test]
fn json_array_length_with_path() {
    let out = t("SELECT json_array_length(extra_data, '$.items') FROM t");
    let low = out.to_lowercase();
    assert!(low.contains("jsonb_array_length"), "{}", out);
    assert_pg(&out);
}

#[test]
fn json_set_rewrites() {
    let out = t("SELECT json_set(extra_data, '$.status', 'ok') FROM t");
    let low = out.to_lowercase();
    assert!(low.contains("jsonb_set"), "{}", out);
    assert!(low.contains("string_to_array"), "{}", out);
    assert!(low.contains("to_jsonb"), "{}", out);
    assert_pg(&out);
}

#[test]
fn json_insert_and_replace_rewrite() {
    let insert_out = t("SELECT json_insert(extra_data, '$.status', 'ok') FROM t");
    let replace_out = t("SELECT json_replace(extra_data, '$.status', 'ok') FROM t");
    let insert_low = insert_out.to_lowercase();
    let replace_low = replace_out.to_lowercase();
    assert!(insert_low.contains("jsonb_set"), "{}", insert_out);
    assert!(replace_low.contains("jsonb_set"), "{}", replace_out);
    assert!(insert_low.contains("jsonb_path_exists"), "{}", insert_out);
    assert!(replace_low.contains("jsonb_path_exists"), "{}", replace_out);
    assert!(insert_low.contains("case"), "{}", insert_out);
    assert!(replace_low.contains("case"), "{}", replace_out);
    assert_pg(&insert_out);
    assert_pg(&replace_out);
}

#[test]
fn json_quote_rewrites() {
    let out = t("SELECT json_quote(title) FROM t");
    let low = out.to_lowercase();
    assert!(low.contains("to_json"), "{}", out);
    assert!(low.contains("::text"), "{}", out);
    assert_pg(&out);
}
