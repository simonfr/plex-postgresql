use plex_pg_core::translate;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::thread;

fn translate_sql(sql: &str) -> String {
    translate(sql)
        .unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sql, e))
        .sql
}

fn assert_pg(sql: &str) {
    Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .unwrap_or_else(|e| panic!("postgres parse failed for `{}`: {}", sql, e));
}

#[test]
fn transaction_keywords_translate_to_pg_parseable_sql() {
    let statements = [
        "BEGIN",
        "BEGIN IMMEDIATE",
        "BEGIN DEFERRED",
        "BEGIN EXCLUSIVE",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT sp1",
        "RELEASE SAVEPOINT sp1",
        "ROLLBACK TO SAVEPOINT sp1",
        "ROLLBACK TO sp1",
        "BEGIN TRANSACTION",
        "ROLLBACK TRANSACTION",
        "ROLLBACK TRANSACTION TO sp1",
        "RELEASE sp1",
        "END",
        "END TRANSACTION",
    ];

    for stmt in statements {
        let out = translate_sql(stmt);
        assert!(!out.trim().is_empty(), "empty output for `{}`", stmt);
        assert_pg(&out);
    }
}

#[test]
fn transaction_keywords_with_case_whitespace_and_semicolons() {
    let statements = [
        "  begin immediate ;  ",
        "\n\tBeGiN ExClUsIvE;\n",
        "commit ;",
        "  ROLLBACK; ",
        " savepoint   Sp_A ; ",
        " release savepoint Sp_A ; ",
        " rollback to sp_a ; ",
        " end transaction ; ",
    ];

    for stmt in statements {
        let out = translate_sql(stmt);
        assert!(!out.trim().is_empty(), "empty output for `{}`", stmt);
        assert_pg(&out);
    }
}

#[test]
fn savepoint_nested_sequence_is_pg_parseable() {
    let sqlite_sql = "\
        BEGIN IMMEDIATE;\
        SAVEPOINT a;\
        SAVEPOINT b;\
        ROLLBACK TO b;\
        RELEASE SAVEPOINT b;\
        RELEASE SAVEPOINT a;\
        COMMIT;";

    let out = translate_sql(sqlite_sql);
    let low = out.to_lowercase();
    assert!(low.contains("savepoint a"), "{}", out);
    assert!(low.contains("savepoint b"), "{}", out);
    assert!(low.contains("rollback to savepoint b"), "{}", out);
    assert!(low.contains("release savepoint a"), "{}", out);
    assert!(low.contains("commit"), "{}", out);
    assert_pg(&out);
}

#[test]
fn multi_statement_transaction_with_dml_is_pg_parseable() {
    let sqlite_sql = "\
        BEGIN EXCLUSIVE;\
        INSERT INTO tags (id, tag, tag_type) VALUES (999001, 'tx-test', 0);\
        UPDATE tags SET tag='tx-test-2' WHERE id=999001;\
        ROLLBACK;";

    let out = translate_sql(sqlite_sql);
    let low = out.to_lowercase();
    assert!(low.contains("begin"), "{}", out);
    assert!(low.contains("insert into"), "{}", out);
    assert!(low.contains("update tags"), "{}", out);
    assert!(low.contains("rollback"), "{}", out);
    assert_pg(&out);
}

#[test]
fn transaction_leading_comments_are_accepted() {
    let statements = [
        "/* tx */ BEGIN IMMEDIATE;",
        "-- tx\nCOMMIT;",
        "/* tx */ ROLLBACK TO sp1;",
        "/* tx */ RELEASE sp1;",
    ];
    for stmt in statements {
        let out = translate_sql(stmt);
        assert!(!out.trim().is_empty(), "{}", stmt);
        assert_pg(&out);
    }
}

#[test]
fn transaction_bad_statement_error_then_recovery() {
    let bad = translate("BEGIN; SELECT FROM; ROLLBACK");
    assert!(bad.is_err(), "expected parse error");

    let good = translate("BEGIN; ROLLBACK").expect("recovery translate should work");
    assert_pg(&good.sql);
}

#[test]
fn transaction_translation_is_thread_safe_under_parallel_calls() {
    let inputs = [
        "BEGIN IMMEDIATE; SAVEPOINT a; ROLLBACK TO a; RELEASE a; COMMIT;",
        "BEGIN EXCLUSIVE; INSERT INTO tags (id, tag, tag_type) VALUES (1, 'x', 0); ROLLBACK;",
        "BEGIN TRANSACTION; SAVEPOINT s1; RELEASE s1; END;",
        "ROLLBACK TO sp1; RELEASE sp1;",
    ];

    let mut handles = Vec::new();
    for _ in 0..8 {
        let batch = inputs;
        handles.push(thread::spawn(move || {
            for sql in batch {
                let out = translate(sql).unwrap_or_else(|e| panic!("{} => {}", sql, e));
                assert_pg(&out.sql);
            }
        }));
    }
    for h in handles {
        h.join().expect("thread failed");
    }
}
