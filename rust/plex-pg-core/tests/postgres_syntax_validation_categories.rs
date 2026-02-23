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
fn category_functions_and_time() {
    let sqls = [
        "SELECT IFNULL(name, 'x') FROM tags",
        "SELECT iif(rating > 8, 'good', 'ok') FROM metadata_items",
        "SELECT typeof(duration) FROM media_items",
        "SELECT datetime('now')",
        "SELECT unixepoch('now')",
        "SELECT strftime('%s','now')",
        "SELECT instr(title, 'war') FROM metadata_items",
        "SELECT substr(title, 1, 10) FROM metadata_items",
        "SELECT last_insert_rowid()",
    ];
    assert_category("functions_and_time", &sqls);
}

#[test]
fn category_placeholders_and_params() {
    let sqls = [
        "SELECT * FROM metadata_items WHERE id = ?",
        "SELECT * FROM metadata_items WHERE guid = :guid",
        "SELECT * FROM metadata_items WHERE id = :id AND title = ?",
        "UPDATE metadata_items SET title = :title WHERE id = :id",
        "INSERT INTO tags(tag, tag_type) VALUES(:tag, :kind)",
    ];
    assert_category("placeholders_and_params", &sqls);
}

#[test]
fn category_upsert_variants() {
    let sqls = [
        "INSERT OR REPLACE INTO tags (id, tag, tag_type) VALUES (1, 'Action', 0)",
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('20260101')",
        "REPLACE INTO preferences (id, name, value) VALUES (1, 'foo', 'bar')",
        "INSERT OR REPLACE INTO plex.preferences (id, name, value) VALUES (1, 'foo', 'bar')",
        "INSERT OR REPLACE INTO statistics_bandwidth (id, account_id, device_id, timespan, at, lan, bytes) VALUES (1, 2, 3, 4, 5, 1, 99)",
        "INSERT OR REPLACE INTO some_unknown_table (id, data) VALUES (1, 'x')",
        "INSERT INTO tags(id, tag) VALUES(1, 'x') ON CONFLICT(id) DO UPDATE SET tag = excluded.tag",
    ];
    assert_category("upsert_variants", &sqls);
}

#[test]
fn category_keyword_and_preprocess_rules() {
    let sqls = [
        "BEGIN IMMEDIATE",
        "BEGIN EXCLUSIVE",
        "SELECT * FROM tags WHERE name GLOB '*test*'",
        "SELECT * FROM t WHERE id in ()",
        "SELECT name FROM sqlite_master WHERE type = 'table'",
        "SELECT count(*) FROM metadata_items GROUP BY NULL",
        "SELECT * FROM metadata_items INDEXED BY sqlite_autoindex_metadata_items_1",
    ];
    assert_category("keyword_and_preprocess_rules", &sqls);
}

#[test]
fn category_groupby_distinct_and_ordering() {
    let sqls = [
        "SELECT id, name FROM t GROUP BY id",
        "SELECT DISTINCT id, name FROM t GROUP BY id",
        "SELECT id, count(*) as cnt FROM t GROUP BY id HAVING cnt = 0",
        "SELECT id, name FROM t GROUP BY id ORDER BY name",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a",
        "SELECT id, title FROM metadata_items WHERE id IS NULL ORDER BY id IS NULL, id",
    ];
    assert_category("groupby_distinct_and_ordering", &sqls);
}

#[test]
fn category_query_fixups_and_collation() {
    let sqls = [
        "SELECT * FROM t WHERE name COLLATE NOCASE = 'Test'",
        "SELECT * FROM t WHERE name LIKE '%test%' COLLATE NOCASE",
        "SELECT * FROM t ORDER BY name COLLATE NOCASE",
        "SELECT max(a, b) FROM t",
        "SELECT min(a, b) FROM t",
        "SELECT * FROM t WHERE extra_data ->> '$.pv:version' < $3",
        "SELECT CASE WHEN x THEN 1 ELSE 0 END FROM t",
    ];
    assert_category("query_fixups_and_collation", &sqls);
}

#[test]
fn category_transaction_control() {
    let sqls = [
        "BEGIN",
        "BEGIN IMMEDIATE",
        "BEGIN DEFERRED",
        "BEGIN EXCLUSIVE",
        "BEGIN TRANSACTION",
        "COMMIT",
        "END",
        "END TRANSACTION",
        "ROLLBACK",
        "ROLLBACK TRANSACTION",
        "SAVEPOINT sp1",
        "RELEASE sp1",
        "RELEASE SAVEPOINT sp1",
        "ROLLBACK TO SAVEPOINT sp1",
        "ROLLBACK TO sp1",
        "ROLLBACK TRANSACTION TO sp1",
        "/* tx */ BEGIN IMMEDIATE; COMMIT",
        "-- tx\nBEGIN; ROLLBACK",
    ];
    assert_category("transaction_control", &sqls);
}

#[test]
fn category_joins_subqueries_and_windows() {
    let sqls = [
        "SELECT * FROM (SELECT id FROM metadata_items)",
        "SELECT t.id, (SELECT count(*) FROM t2 WHERE t2.pid = t.id) AS cnt FROM t GROUP BY t.id",
        "SELECT dense_rank() OVER (PARTITION BY a ORDER BY b) FROM t",
        "SELECT row_number() OVER (ORDER BY id) FROM metadata_items",
        "SELECT rank() OVER (ORDER BY score DESC) FROM statistics_media",
        "SELECT t.id FROM t LEFT JOIN t2 ON t.id = t2.t_id",
    ];
    assert_category("joins_subqueries_and_windows", &sqls);
}

#[test]
fn category_ddl_and_types() {
    let sqls = [
        "CREATE TABLE foo (id INTEGER PRIMARY KEY AUTOINCREMENT, data BLOB, dt DATETIME, flag INTEGER DEFAULT 't')",
        "ALTER TABLE foo ADD COLUMN bar TEXT",
        "CREATE UNIQUE INDEX idx_foo_data ON foo(data)",
        "CREATE TABLE `mixedCase` (`itemId` INTEGER, `itemTitle` TEXT)",
    ];
    assert_category("ddl_and_types", &sqls);
}
