use plex_pg_core::translate;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

fn assert_pg(sqlite_sql: &str) {
    let out = translate(sqlite_sql)
        .unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sqlite_sql, e));
    Parser::parse_sql(&PostgreSqlDialect {}, &out.sql).unwrap_or_else(|e| {
        panic!(
            "postgres parse failed\ninput: {}\noutput: {}\nerror: {}",
            sqlite_sql, out.sql, e
        )
    });
}

#[test]
fn category_upsert_on_conflict_variants() {
    let sqls = [
        "INSERT INTO t(id, v) VALUES(1, 'x') ON CONFLICT(id) DO NOTHING",
        "INSERT INTO t(id, v) VALUES(1, 'x') ON CONFLICT(id) DO UPDATE SET v = excluded.v",
        "INSERT INTO t(id, v) VALUES(1, 'x') ON CONFLICT(id) DO UPDATE SET v = excluded.v WHERE excluded.v IS NOT NULL",
        "INSERT INTO t(id, a, b) VALUES(1, 2, 3) ON CONFLICT(id) DO UPDATE SET a = excluded.a, b = excluded.b",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_ddl_sqlite_options() {
    let sqls = [
        "CREATE TABLE t1(id INTEGER PRIMARY KEY, name TEXT) WITHOUT ROWID",
        "CREATE TABLE t2(id INTEGER PRIMARY KEY, name TEXT) STRICT",
        "CREATE TABLE t3(id INTEGER PRIMARY KEY, name TEXT) WITHOUT ROWID, STRICT",
        "CREATE TABLE t4(id INTEGER PRIMARY KEY, v TEXT GENERATED ALWAYS AS (name) VIRTUAL)",
        "CREATE TABLE t5(id INTEGER PRIMARY KEY, v TEXT, CHECK(length(v) > 0))",
        "CREATE INDEX idx_expr ON t5(lower(v))",
        "CREATE INDEX idx_partial ON t5(v) WHERE v IS NOT NULL",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_cte_window_and_frames() {
    let sqls = [
        "WITH q AS (SELECT id, v FROM t) SELECT * FROM q",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 4) SELECT * FROM r",
        "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
        "SELECT id, avg(v) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
        "SELECT count(*) FILTER (WHERE v > 0) FROM t",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_date_time_and_json_extended() {
    let sqls = [
        "SELECT strftime('%Y-%m-%d', 'now')",
        "SELECT datetime('now', '-7 day')",
        "SELECT unixepoch('now', '-7 day')",
        "SELECT julianday('now')",
        "SELECT total(v) FROM t",
        "SELECT json_object('a', 1, 'b', 2)",
        "SELECT json_array(1, 2, 3)",
        "SELECT json_patch('{\"a\":1}', '{\"b\":2}')",
        "SELECT json_remove('{\"a\":1,\"b\":2}', '$.b')",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_sqlite_utility_statements() {
    let sqls = [
        "EXPLAIN QUERY PLAN SELECT * FROM t",
        "VACUUM",
        "REINDEX",
        "ATTACH DATABASE 'aux.db' AS aux",
        "DETACH DATABASE aux",
        "ANALYZE sqlite_master",
        "ANALYZE t",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_returning_and_alter_table() {
    let sqls = [
        "INSERT INTO t(id, v) VALUES(1, 'x') RETURNING id",
        "UPDATE t SET v = 'y' WHERE id = 1 RETURNING id, v",
        "DELETE FROM t WHERE id = 1 RETURNING id",
        "ALTER TABLE t RENAME TO t_new",
        "ALTER TABLE t RENAME COLUMN old_name TO new_name",
        "ALTER TABLE t DROP COLUMN obsolete_col",
        "ALTER TABLE t ADD COLUMN extra TEXT DEFAULT 'x'",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_views_triggers_and_hints() {
    let sqls = [
        "CREATE VIEW v_t AS SELECT id, v FROM t",
        "DROP VIEW IF EXISTS v_t",
        "CREATE TRIGGER tr_ai AFTER INSERT ON t BEGIN UPDATE t SET v = 'x' WHERE id = NEW.id; END",
        "CREATE TRIGGER tr_bi BEFORE INSERT ON t BEGIN SELECT RAISE(ABORT, 'boom'); END",
        "SELECT * FROM t NOT INDEXED WHERE id = 1",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_string_pattern_and_aggregates() {
    let sqls = [
        "SELECT printf('%s-%d', name, id) FROM t",
        "SELECT group_concat(v) FROM t",
        "SELECT group_concat(v, '|') FROM t",
        "SELECT replace(name, 'a', 'b') FROM t",
        "SELECT trim(name), ltrim(name), rtrim(name) FROM t",
        "SELECT name FROM t WHERE name LIKE '%ab\\_%' ESCAPE '\\\\'",
        "SELECT name FROM t WHERE name REGEXP 'ab.*'",
        "SELECT name FROM t WHERE name NOT REGEXP 'ab.*'",
        "SELECT json_group_array(v) FROM t",
        "SELECT json_group_object(k, v) FROM t",
    ];
    for s in sqls {
        assert_pg(s);
    }
}
