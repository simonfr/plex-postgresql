# Translator Test Tagging (Local Draft)

Entry point: `docs/translator/README.md`

## Tag Set
- `subset/core`
- `subset/json`
- `subset/fts`
- `subset/pragma`
- `subset/txn`
- `subset/ddl-lite`
- `validation/output`
- `rewrite/idempotence`
- `rewrite/placeholders`
- `rewrite/groupby`
- `rewrite/distinct-orderby`
- `compat/backticks`
- `compat/aliases`
- `error/conn-retry-signal`

## Naming Convention
- Test name prefix: `<tag>__<short_case>`
- Example: `rewrite/idempotence__groupby_projection_stable`
- Rust test functions use a sanitized tag prefix: replace `/` and `-` with `_`
  Example: `rewrite_idempotence__groupby_projection_stable`

## Minimal Matrix
- Per rewrite family: positive + negative + edge case
- Per subset family: happy path + known gap case
- Per high-risk rule: idempotence check + postgres parse check
