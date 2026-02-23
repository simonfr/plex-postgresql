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
fn category_with_dml_returning_combos() {
    let sqls = [
        "WITH src AS (SELECT 1 AS id, 'x' AS v) INSERT INTO t(id, v) SELECT id, v FROM src RETURNING id",
        "WITH updated AS (SELECT id FROM t WHERE id = 1) UPDATE t SET v = 'y' WHERE id IN (SELECT id FROM updated) RETURNING id, v",
        "WITH doomed AS (SELECT id FROM t WHERE id = 1) DELETE FROM t WHERE id IN (SELECT id FROM doomed) RETURNING id",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_trigger_forms_and_raise() {
    let sqls = [
        "CREATE TRIGGER tr_bu BEFORE UPDATE ON t FOR EACH ROW WHEN NEW.v IS NOT NULL BEGIN SELECT RAISE(FAIL, 'bad'); END",
        "CREATE TRIGGER tr_au AFTER UPDATE ON t BEGIN UPDATE t SET v = NEW.v WHERE id = OLD.id; END",
        "CREATE VIEW vv AS SELECT id, v FROM t",
        "CREATE TRIGGER tr_iov INSTEAD OF UPDATE ON vv BEGIN SELECT RAISE(IGNORE); END",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_constraint_on_conflict_nuances() {
    let sqls = [
        "CREATE TABLE c1(id INTEGER PRIMARY KEY ON CONFLICT REPLACE, v TEXT UNIQUE ON CONFLICT IGNORE)",
        "CREATE TABLE c2(a TEXT, b TEXT, UNIQUE(a, b) ON CONFLICT ABORT)",
        "CREATE TABLE c3(id INTEGER, v TEXT, CONSTRAINT uq UNIQUE(v) ON CONFLICT FAIL)",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_json_wildcards_filters_and_paths() {
    let sqls = [
        "SELECT json_extract(extra_data, '$.items[*].id') FROM t",
        "SELECT json_type(extra_data, '$.items ? (@.id > 1)') FROM t",
        "SELECT json_array_length(extra_data, '$.items[*]') FROM t",
        "SELECT json_set(extra_data, '$.items[0].name', 'abc') FROM t",
        "SELECT json_remove(extra_data, '$.items[0].obsolete') FROM t",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_type_affinity_and_cast_edges() {
    let sqls = [
        "SELECT CAST(v AS INTEGER) = 1 FROM t",
        "SELECT CAST(v AS REAL) > 1.5 FROM t",
        "SELECT id = '123' FROM t",
        "SELECT title = 123 FROM metadata_items",
        "SELECT id IN ('1', '2', '3') FROM t",
        "SELECT v + '1' FROM t",
    ];
    for s in sqls {
        assert_pg(s);
    }
}

#[test]
fn category_attach_schema_refs_and_misc() {
    let sqls = [
        "ATTACH DATABASE 'a.db' AS aux",
        "SELECT * FROM aux.some_table",
        "DETACH DATABASE aux",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE id = 1",
    ];
    for s in sqls {
        assert_pg(s);
    }
}
