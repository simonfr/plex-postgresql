# Known Gaps

These are the areas where the translator is intentionally partial or where semantics still differ from native SQLite.

## Partial Areas
- JSON wildcard and exotic path forms may fall back to guarded PostgreSQL JSONPath rewrites instead of exact SQLite behavior.
- FTS rewrites are tuned to observed Plex queries rather than the full FTS3/4/5 grammar.
- DDL normalization is intentionally narrow and does not aim to cover arbitrary SQLite schema statements.
- Transaction keywords are translated for Plex workloads, but lock timing and savepoint behavior are not a full SQLite clone.

## Intentional Non-Support
- Virtual table modules such as `spellfix`, `rtree`, and custom tokenizer extensions
- SQLite extension loading and extension-specific PRAGMAs
- Table-valued helpers such as `json_tree`
- SQLite-specific collation and Unicode extension behaviors that do not map safely to PostgreSQL

## Engineering Rule
- New unsupported syntax should be documented here when it is discovered in the field or added as a failing compatibility test.
- When a gap becomes supported, remove it here and add coverage in `docs/TRANSLATOR_COVERAGE.md`.
