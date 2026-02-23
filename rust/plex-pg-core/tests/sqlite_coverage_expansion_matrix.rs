use plex_pg_core::translate;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

fn assert_translates_to_valid_pg(sqlite_sql: &str) {
    let out = translate(sqlite_sql).unwrap_or_else(|e| {
        panic!("translate failed for input `{}`: {}", sqlite_sql, e);
    });

    Parser::parse_sql(&PostgreSqlDialect {}, &out.sql).unwrap_or_else(|e| {
        panic!(
            "postgres parse failed\ninput: {}\noutput: {}\nerror: {}",
            sqlite_sql, out.sql, e
        )
    });
}

fn assert_category(name: &str, sqls: &[&str]) {
    for s in sqls {
        assert_translates_to_valid_pg(s);
    }
    assert!(!sqls.is_empty(), "category `{}` should not be empty", name);
}

#[test]
fn expansion_json_matrix() {
    let sqls = [
        "SELECT json_extract(extra_data, '$.a[12].b.c') FROM t",
        "SELECT json_extract(extra_data, '$[\"a.b\"][0]') FROM t",
        "SELECT json_extract(extra_data, '$.items[-1]') FROM t",
        "SELECT json_type(extra_data, '$.a[0]') FROM t",
        "SELECT json_array_length(extra_data, '$.items[2].children') FROM t",
        "SELECT json_set(extra_data, '$.a', 1, '$.b', 'x') FROM t",
        "SELECT json_insert(extra_data, '$.a.b', 1) FROM t",
        "SELECT json_replace(extra_data, '$.a.b', 1) FROM t",
        "SELECT key, value FROM json_each(extra_data)",
        "SELECT key, value FROM json_tree(extra_data)",
        "SELECT extra_data -> '$.a' FROM t",
        "SELECT extra_data ->> '$.a.b' FROM t",
    ];
    assert_category("expansion_json_matrix", &sqls);
}

#[test]
fn expansion_backticks_and_aliases() {
    let sqls = [
        "SELECT `m`.`id`, `m`.`title` FROM `metadata_items` AS `m`",
        "SELECT `id` itemAlias FROM `t` t0",
        "SELECT `t`.`id` FROM `t` JOIN `t2` ON `t`.`id` = `t2`.`id`",
        "SELECT `t`.`id` AS `IdMixed` FROM `t` ORDER BY `IdMixed`",
        "SELECT * FROM (SELECT `id` AS `innerId` FROM `t`) AS `q`",
        "CREATE TABLE `mixedCaseTable` (`itemId` INTEGER, `itemTitle` TEXT)",
        "CREATE INDEX `idx_mixed_title` ON `mixedCaseTable`(`itemTitle`)",
    ];
    assert_category("expansion_backticks_and_aliases", &sqls);
}

#[test]
fn expansion_groupby_distinct_having() {
    let sqls = [
        "SELECT a, count(*) AS c FROM t GROUP BY a HAVING c > 0",
        "SELECT a + 1 AS k, count(*) FROM t GROUP BY k",
        "SELECT DISTINCT a, b FROM t ORDER BY b, a",
        "SELECT DISTINCT a FROM t ORDER BY a",
        "SELECT a, sum(v) s FROM t GROUP BY a ORDER BY s DESC",
        "SELECT a, count(*) FROM t GROUP BY 1",
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1",
    ];
    assert_category("expansion_groupby_distinct_having", &sqls);
}

#[test]
fn expansion_cte_windows_and_subqueries() {
    let sqls = [
        "WITH q AS (SELECT id FROM t) SELECT * FROM q",
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x < 5) SELECT * FROM cnt",
        "SELECT id, row_number() OVER (PARTITION BY kind ORDER BY id) FROM t",
        "SELECT id, lag(score) OVER (PARTITION BY grp ORDER BY ts) FROM t",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.id = t.id)",
        "SELECT id FROM t WHERE id IN (SELECT id FROM t2)",
    ];
    assert_category("expansion_cte_windows_and_subqueries", &sqls);
}

#[test]
fn expansion_transaction_variants() {
    let sqls = [
        "BEGIN TRANSACTION; SAVEPOINT spx; RELEASE spx; COMMIT",
        "BEGIN; SAVEPOINT s1; ROLLBACK TO s1; RELEASE SAVEPOINT s1; COMMIT",
        "BEGIN IMMEDIATE; INSERT INTO t(id) VALUES(1); ROLLBACK",
        "/*x*/ BEGIN EXCLUSIVE; -- y\nEND",
    ];
    assert_category("expansion_transaction_variants", &sqls);
}

#[test]
fn expansion_pragma_and_misc_sqlite_compat() {
    let sqls = [
        "PRAGMA foreign_keys",
        "PRAGMA foreign_keys = ON",
        "PRAGMA journal_mode=WAL",
        "PRAGMA journal_mode",
        "PRAGMA main.busy_timeout = 5000",
        "PRAGMA synchronous",
        "PRAGMA temp_store=MEMORY",
        "PRAGMA temp_store",
        "PRAGMA cache_size=-4000",
        "PRAGMA cache_size",
        "PRAGMA locking_mode=EXCLUSIVE",
        "PRAGMA locking_mode",
        "PRAGMA wal_autocheckpoint=200",
        "PRAGMA wal_autocheckpoint",
        "PRAGMA mmap_size=1048576",
        "PRAGMA mmap_size",
        "PRAGMA page_size=8192",
        "PRAGMA page_size",
        "PRAGMA auto_vacuum=INCREMENTAL",
        "PRAGMA auto_vacuum",
    ];
    assert_category("expansion_pragma_and_misc_sqlite_compat", &sqls);
}
