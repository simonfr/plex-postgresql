# SQLite Compatibility Harness

Run from `rust/plex-pg-core`:

```bash
cargo run --bin sqlite_compat_harness -- \
  --suite tests/sqllogictest \
  --allowlist tests/sqllogictest/known_divergences.txt
```

Current category suites:

- `smoke.slt`
- `aliases_backticks.slt`
- `groupby.slt`
- `distinct_order.slt`
- `json.slt`
- `transactions.slt`
- `cte_window.slt`
- `joins_subqueries.slt`
- `upsert_replace.slt`
- `pragma_utility.slt`
- `json_edges.slt`
- `functions_time.slt`
- `set_ops.slt`
- `collate_nocase.slt`
- `ddl_alter.slt`
- `subquery_exists.slt`
- `scalar_misc.slt`
- `window_frames.slt`
- `nested_cte.slt`
- `upsert_conflict_complex.slt`
- `json_mutations.slt`
- `order_nulls.slt`
- `in_empty_and_lists.slt`
- `like_glob.slt`
- `datetime_strftime_unixepoch.slt`
- `join_edgecases.slt`
- `update_delete_limit.slt`
- `cast_affinity.slt`
- `create_index_variants.slt`
- `json_operators_advanced.slt`
- `transaction_modes.slt`
- `params_placeholders.slt`
- `params_reuse_and_order.slt`
- `fts_match_advanced.slt`
- `json_each_tree_shape.slt`
- `alter_table_rename_and_defaults.slt`
- `generated_columns_and_indexes.slt`
- `collation_locale_edges.slt`
- `transaction_rollback_savepoint.slt`
- `concurrency_semantics.slt`
- `plex_real_queries_sample.slt`
- `null_three_valued_logic.slt`
- `numeric_precision_rounding.slt`
- `blob_and_hex_behavior.slt`
- `cte_recursive.slt`
- `window_advanced_rank_dense_ntile.slt`

Optional flags:

- `--pg-url "<postgres connection string>"`
- `--pg-schema "slt_tmp_schema"`
- `--dry-run` (parse only, no DB execution)
- `--keep-schema` (do not drop temp schema)

Environment fallback (when `--pg-url` not passed):

- `PLEX_PG_HOST`, `PLEX_PG_PORT`, `PLEX_PG_DATABASE`, `PLEX_PG_USER`, `PLEX_PG_PASSWORD`
- or single `SLT_PG_URL`
