#![allow(non_snake_case)]

mod self_join_tests {
    use super::super::preprocess;

    // ── Shape A tests ──────────────────────────────────────────────────────────

    /// Original failing pattern: FROM metadata_item_settings + aliased parents join
    /// + UNALIASED metadata_items join.  The unaliased join must get AS mi and all
    /// metadata_items.<col> refs must become mi.<col>.
    #[test]
    fn compat_aliases__preprocess_rewrites_metadata_items_self_join_alias_refs() {
        // Shape A: unaliased second join that needs AS mi
        let input = concat!(
            "SELECT metadata_items.id, metadata_items.title ",
            "FROM metadata_item_settings ",
            "JOIN metadata_items AS parents ON parents.id = metadata_items.parent_id ",
            "JOIN metadata_items ON metadata_items.id = metadata_item_settings.metadata_item_id ",
            "WHERE metadata_items.library_section_id = 1"
        );
        let out = preprocess(input);
        // The unaliased JOIN should have AS mi now
        assert!(
            out.to_lowercase().contains("join metadata_items as mi"),
            "unaliased join should get AS mi, out={}",
            out
        );
        // All metadata_items.<col> should be mi.<col>
        assert!(
            !out.to_lowercase().contains("metadata_items."),
            "no bare metadata_items. refs should remain, out={}",
            out
        );
        // The aliased parents join must stay
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as parents"),
            "parents alias must be preserved, out={}",
            out
        );
        // mi. refs are present
        assert!(
            out.to_lowercase().contains("mi.id"),
            "mi.id should appear, out={}",
            out
        );
    }

    /// Legacy test: all joins already aliased — nothing should change (no unaliased join).
    #[test]
    fn compat_aliases__preprocess_no_rewrite_when_all_joins_aliased() {
        let input = concat!(
            "select metadata_items.id from metadata_item_settings ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id"
        );
        let out = preprocess(input);
        // When all joins are aliased there is no unaliased join to fix, so the
        // function should return the input unchanged (aside from other preprocess steps).
        // Both aliases must still be present.
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as parents"),
            "parents alias must stay, out={}",
            out
        );
        assert!(
            out.to_lowercase()
                .contains("join metadata_items as grandparents"),
            "grandparents alias must stay, out={}",
            out
        );
        // No spurious AS mi should have been injected
        assert!(
            !out.to_lowercase().contains("as mi"),
            "no AS mi should appear when all joins aliased, out={}",
            out
        );
    }

    /// Shape B (from-metadata_items root): no rewrite should happen — base table
    /// IS metadata_items so bare refs are valid in PostgreSQL.
    #[test]
    fn compat_aliases__preprocess_no_rewrite_for_shape_b_metadata_items_root() {
        let input = concat!(
            "select metadata_items.id from metadata_items ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id ",
            "where metadata_items.library_section_id in (2)"
        );
        let out = preprocess(input);
        // FROM is metadata_items, NOT metadata_item_settings, so no rewrite fires.
        assert!(
            !out.to_lowercase().contains("as mi"),
            "no AS mi for shape-B query, out={}",
            out
        );
    }
}
mod tests {
    use super::super::{preprocess, Ordering, TEST_STRICT_PRAGMA_OVERRIDE};
    use crate::translate;
    use crate::test_utils::env_lock;

    #[test]
    fn subset_txn__keyword_begin_immediate() {
        let r = translate("BEGIN IMMEDIATE").unwrap();
        assert!(!r.sql.to_uppercase().contains("IMMEDIATE"));
        assert!(!r.sql.to_uppercase().contains("DEFERRED"));
        assert!(!r.sql.to_uppercase().contains("EXCLUSIVE"));
    }

    #[test]
    fn subset_txn__keyword_begin_deferred() {
        let r = translate("BEGIN DEFERRED").unwrap();
        assert!(!r.sql.to_uppercase().contains("DEFERRED"));
    }

    #[test]
    fn subset_txn__keyword_begin_exclusive() {
        let r = translate("BEGIN EXCLUSIVE").unwrap();
        assert!(!r.sql.to_uppercase().contains("EXCLUSIVE"));
    }

    #[test]
    fn subset_txn__keyword_end_to_commit() {
        let r = translate("END").unwrap();
        assert!(r.sql.to_uppercase().contains("COMMIT"));
    }

    #[test]
    fn subset_txn__keyword_release_without_savepoint_keyword() {
        let r = translate("RELEASE sp1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("RELEASE SAVEPOINT SP1"), "{}", r.sql);
    }

    #[test]
    fn subset_txn__keyword_rollback_transaction_to() {
        let r = translate("ROLLBACK TRANSACTION TO sp1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("ROLLBACK TO SAVEPOINT SP1"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_glob_wildcard() {
        let r = translate("SELECT * FROM t WHERE name GLOB '*test*'").unwrap();
        assert!(r.sql.to_uppercase().contains("ILIKE") || r.sql.to_uppercase().contains("LIKE"));
        assert!(!r.sql.to_uppercase().contains(" GLOB "));
    }

    #[test]
    fn subset_core__keyword_indexed_by_removed() {
        let r = translate("SELECT * FROM metadata_items INDEXED BY idx_title WHERE title = 'test'")
            .unwrap();
        assert!(!r.sql.to_uppercase().contains("INDEXED BY"));
        assert!(r.sql.to_uppercase().contains("WHERE"));
    }

    #[test]
    fn subset_core__keyword_not_indexed_removed() {
        let r = translate("SELECT * FROM metadata_items NOT INDEXED WHERE id = 1").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("NOT INDEXED"), "{}", r.sql);
        assert!(up.contains("WHERE"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_sqlite_master_replaced() {
        let r = translate("SELECT name FROM sqlite_master WHERE type='table'").unwrap();
        assert!(
            r.sql.to_lowercase().contains("information_schema")
                || r.sql.to_lowercase().contains("pg_")
        );
        assert!(!r.sql.to_lowercase().contains("sqlite_master"));
    }

    #[test]
    fn subset_core__keyword_empty_in_list() {
        let r = translate("SELECT * FROM tags WHERE id IN ()").unwrap();
        assert!(!r.sql.contains("IN ()"));
        assert!(r.sql.to_uppercase().contains("IN") && r.sql.to_uppercase().contains("SELECT"));
    }

    #[test]
    fn subset_core__keyword_group_by_null_removed() {
        let r = translate("SELECT count(*) FROM metadata_items GROUP BY NULL").unwrap();
        assert!(!r.sql.to_uppercase().contains("GROUP BY NULL"));
    }

    #[test]
    fn subset_pragma__keyword_pragma_read_is_mapped_to_select() {
        let r = translate("PRAGMA foreign_keys").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SELECT 1 AS FOREIGN_KEYS"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_set_is_mapped_to_select_one() {
        let r = translate("PRAGMA journal_mode=WAL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.JOURNAL_MODE"), "{}", r.sql);
        assert!(!up.contains("PRAGMA"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_schema_prefix_is_supported() {
        let r = translate("PRAGMA main.busy_timeout = 5000").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("LOCK_TIMEOUT"), "{}", r.sql);
        assert!(!up.contains("PRAGMA"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_unknown_is_removed() {
        let r = translate("PRAGMA this_is_unknown").unwrap();
        assert!(r.sql.trim().is_empty(), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_busy_timeout_read_uses_current_setting() {
        let r = translate("PRAGMA busy_timeout").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("LOCK_TIMEOUT"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_synchronous_set_uses_set_config() {
        let r = translate("PRAGMA synchronous = FULL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("SYNCHRONOUS_COMMIT"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_temp_store_set_uses_session_setting() {
        let r = translate("PRAGMA temp_store=MEMORY").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.TEMP_STORE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_temp_store_read_uses_current_setting() {
        let r = translate("PRAGMA temp_store").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.TEMP_STORE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_cache_size_set_uses_session_setting() {
        let r = translate("PRAGMA cache_size=-4000").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.CACHE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_cache_size_read_uses_current_setting() {
        let r = translate("PRAGMA cache_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.CACHE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_locking_mode_set_uses_session_setting() {
        let r = translate("PRAGMA locking_mode=EXCLUSIVE").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.LOCKING_MODE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_locking_mode_read_uses_current_setting() {
        let r = translate("PRAGMA locking_mode").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.LOCKING_MODE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_wal_autocheckpoint_set_uses_session_setting() {
        let r = translate("PRAGMA wal_autocheckpoint=200").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.WAL_AUTOCHECKPOINT"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_wal_autocheckpoint_read_uses_current_setting() {
        let r = translate("PRAGMA wal_autocheckpoint").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.WAL_AUTOCHECKPOINT"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_mmap_size_set_uses_session_setting() {
        let r = translate("PRAGMA mmap_size=1048576").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.MMAP_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_mmap_size_read_uses_current_setting() {
        let r = translate("PRAGMA mmap_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.MMAP_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_page_size_set_uses_session_setting() {
        let r = translate("PRAGMA page_size=8192").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.PAGE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_page_size_read_uses_current_setting() {
        let r = translate("PRAGMA page_size").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.PAGE_SIZE"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_auto_vacuum_set_uses_session_setting() {
        let r = translate("PRAGMA auto_vacuum=INCREMENTAL").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("SET_CONFIG"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.AUTO_VACUUM"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_auto_vacuum_read_uses_current_setting() {
        let r = translate("PRAGMA auto_vacuum").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.contains("CURRENT_SETTING"), "{}", r.sql);
        assert!(up.contains("PLEX.SQLITE.AUTO_VACUUM"), "{}", r.sql);
    }

    #[test]
    fn subset_pragma__keyword_pragma_strict_mode_causes_translation_failure_for_unknown() {
        let _guard = env_lock().lock().unwrap();
        TEST_STRICT_PRAGMA_OVERRIDE.store(1, Ordering::Relaxed);
        let result = translate("PRAGMA totally_unknown_pragma");
        TEST_STRICT_PRAGMA_OVERRIDE.store(-1, Ordering::Relaxed);
        assert!(result.is_err(), "strict pragma mode should fail unknown PRAGMA");
    }

    #[test]
    fn subset_core__keyword_explain_query_plan_rewritten_to_explain() {
        let r = translate("EXPLAIN QUERY PLAN SELECT * FROM t").unwrap();
        let up = r.sql.to_uppercase();
        assert!(up.starts_with("EXPLAIN "), "{}", r.sql);
        assert!(!up.contains("QUERY PLAN"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_vacuum_rewritten_to_select_one() {
        let r = translate("VACUUM").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_reindex_rewritten_to_select_one() {
        let r = translate("REINDEX").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_attach_database_rewritten_to_select_one() {
        let r = translate("ATTACH DATABASE 'x.db' AS aux").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_detach_database_rewritten_to_select_one() {
        let r = translate("DETACH DATABASE aux").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_analyze_sqlite_internal_rewritten_to_select_one() {
        let r = translate("ANALYZE sqlite_master").unwrap();
        assert_eq!(r.sql.trim().to_uppercase(), "SELECT 1");
    }

    #[test]
    fn subset_core__keyword_create_table_without_rowid_strict_stripped() {
        let r = translate("CREATE TABLE t(id INTEGER PRIMARY KEY) WITHOUT ROWID, STRICT").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("WITHOUT ROWID"), "{}", r.sql);
        assert!(!up.contains("STRICT"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_regexp_operator_rewritten() {
        let r = translate("SELECT * FROM t WHERE name REGEXP 'ab.*'").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("REGEXP"), "{}", r.sql);
        assert!(r.sql.contains('~'), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_not_regexp_operator_rewritten() {
        let r = translate("SELECT * FROM t WHERE name NOT REGEXP 'ab.*'").unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("REGEXP"), "{}", r.sql);
        assert!(r.sql.contains("!~"), "{}", r.sql);
    }

    #[test]
    fn subset_core__keyword_raise_function_rewritten_to_null() {
        let r = translate(
            "CREATE TRIGGER tr_bi BEFORE INSERT ON t BEGIN SELECT RAISE(ABORT, 'boom'); END",
        )
        .unwrap();
        let up = r.sql.to_uppercase();
        assert!(!up.contains("RAISE("), "{}", r.sql);
        assert!(up.contains("NULL"), "{}", r.sql);
    }

    // ── COLLATE stripping tests ──────────────────────────────────────────────

    #[test]
    fn compat_aliases__preprocess_strips_icu_root_in_order_by() {
        // This is the exact pattern that was causing parse failures in production
        let sql = "select metadata_items.id from metadata_items where metadata_items.library_section_id in (1) order by metadata_items.added_at desc, metadata_items.title_sort collate icu_root asc, metadata_items.id asc";
        let r = translate(sql).unwrap();
        let out = r.sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "icu_root collation should be stripped, got: {}",
            r.sql
        );
        assert!(
            out.contains("order by"),
            "ORDER BY should still be present, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_strips_icu_root_multiple_order_by_cols() {
        // Multiple COLLATE icu_root in same ORDER BY (from production query)
        let sql = "select id from metadata_items order by added_at desc, grandparents.title_sort collate icu_root asc, metadata_items.title_sort collate icu_root asc, metadata_items.id asc";
        let r = translate(sql).unwrap();
        let out = r.sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "All icu_root collations should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_nocase_handled_at_ast_level() {
        // COLLATE NOCASE is left by pre-parse stripping; the AST-level handler in
        // query.rs converts standalone NOCASE in ORDER BY to LOWER(expr).
        let sql = "SELECT * FROM t ORDER BY name COLLATE NOCASE ASC";
        let r = translate(sql).unwrap();
        let out = r.sql.to_uppercase();
        // The final output should not contain raw COLLATE NOCASE
        assert!(
            !out.contains("COLLATE NOCASE"),
            "COLLATE NOCASE should be handled (converted to LOWER or stripped), got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_strips_rtrim_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE RTRIM";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE RTRIM"),
            "RTRIM collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_strips_binary_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE BINARY";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE BINARY"),
            "BINARY collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_strips_unicode_collation() {
        let sql = "SELECT * FROM t ORDER BY name COLLATE UNICODE";
        let r = translate(sql).unwrap();
        assert!(
            !r.sql.to_uppercase().contains("COLLATE UNICODE"),
            "UNICODE collation should be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_collate_not_stripped_inside_string() {
        // A string literal containing 'COLLATE icu_root' must not be touched
        let sql = "SELECT 'COLLATE icu_root' FROM t";
        let r = translate(sql).unwrap();
        assert!(
            r.sql.contains("COLLATE icu_root"),
            "Collate inside string literal should not be stripped, got: {}",
            r.sql
        );
    }

    #[test]
    fn compat_aliases__preprocess_long_query_collate_icu_parse_succeeds() {
        // Regression test: long query with COLLATE icu_root used to fail at parse time
        let sql = concat!(
            "select metadata_items.id from metadata_items ",
            "join metadata_items as parents on parents.id=metadata_items.parent_id ",
            "join metadata_items as grandparents on grandparents.id=parents.parent_id ",
            "where metadata_items.library_section_id in (2) ",
            "and (metadata_items.metadata_type=4 and metadata_items.added_at>1000000) ",
            "order by metadata_items.added_at desc, ",
            "grandparents.title_sort collate icu_root asc, ",
            "parents.`index` IS NULL, parents.`index` asc, ",
            "metadata_items.`index` IS NULL, metadata_items.`index` asc, ",
            "metadata_items.title_sort collate icu_root asc, ",
            "metadata_items.id asc"
        );
        let r = translate(sql);
        assert!(
            r.is_ok(),
            "Long query with COLLATE icu_root should parse successfully, got: {:?}",
            r.err()
        );
        let out = r.unwrap().sql.to_lowercase();
        assert!(
            !out.contains("collate icu_root"),
            "icu_root collation should be stripped from output, got: {}",
            out
        );
    }

    #[test]
    fn compat_aliases__preprocess_does_not_treat_backtick_limit_identifier_as_limit_clause() {
        let sql = "SELECT pqg.`id`,pqg.`limit`,pqg.`continuous` FROM play_queue_generators pqg WHERE pqg.`type`!=:C1";
        let out = preprocess(sql);
        let low = out.to_ascii_lowercase();
        assert!(
            !low.contains(" offset "),
            "identifier `limit` should not trigger LIMIT/OFFSET rewrite: {}",
            out
        );
        assert!(
            out.contains("`limit`"),
            "backtick identifier should remain unchanged in preprocess output: {}",
            out
        );
    }

}
