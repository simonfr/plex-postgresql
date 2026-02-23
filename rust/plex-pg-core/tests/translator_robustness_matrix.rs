use plex_pg_core::translate;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

fn tr(sql: &str) -> String {
    translate(sql)
        .unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sql, e))
        .sql
}

fn assert_pg(sql: &str) {
    Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .unwrap_or_else(|e| panic!("postgres parse failed for `{}`: {}", sql, e));
}

#[test]
fn backtick_identifier_matrix() {
    let cases = [
        "SELECT `id` FROM `t`",
        "SELECT `order` FROM `t`",
        "SELECT `itemId` AS `itemAlias` FROM `mixedCaseTable`",
        "SELECT `state` FROM `queue_items` WHERE `state` = 1",
    ];
    for sql in cases {
        let out = tr(sql);
        assert!(!out.contains('`'), "backtick remained for `{}` => {}", sql, out);
        assert_pg(&out);
    }
}

#[test]
fn alias_and_subquery_matrix() {
    let cases = [
        "SELECT id AS myAlias FROM t",
        "SELECT t.id AS itemId FROM t",
        "SELECT * FROM (SELECT id FROM t) WHERE id > 0",
        "SELECT a.id FROM (SELECT id FROM t) a",
    ];
    for sql in cases {
        let out = tr(sql);
        assert_pg(&out);
    }
}

#[test]
fn groupby_nulls_first_matrix() {
    let out = tr("SELECT a, count(*) FROM t GROUP BY a");
    let low = out.to_lowercase();
    assert!(low.contains("group by"), "{}", out);
    assert!(low.contains("order by"), "{}", out);
    assert!(low.contains("nulls first"), "{}", out);
    assert_pg(&out);
}

#[test]
fn distinct_orderby_matrix() {
    let out = tr("SELECT DISTINCT id FROM t GROUP BY id ORDER BY count(*)");
    let low = out.to_lowercase();
    assert!(!low.contains("select distinct"), "{}", out);
    assert_pg(&out);

    let out2 = tr("SELECT DISTINCT (id) FROM t ORDER BY title");
    let low2 = out2.to_lowercase();
    assert!(low2.contains("select distinct"), "{}", out2);
    assert!(low2.contains("title"), "{}", out2);
    assert_pg(&out2);
}

