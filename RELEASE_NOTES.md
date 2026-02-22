# Release Notes - v0.9.41

**Release Date:** February 22, 2026

SQL translator and PG modules fully migrated to Rust.

## What changed

### SQL translator (Phase 1) — fully Rust

The entire SQLite-to-PostgreSQL SQL translation pipeline has been rewritten in Rust using `sqlparser-rs` for AST-based transforms. The old C string-manipulation translator (`sql_tr_*.c`, `sql_translator.c`) has been removed.

- Full AST parsing and transformation instead of regex/string matching
- 525 Rust tests covering all translation paths (318 lib + 54 batch1 + 51 batch2 + 42 batch3 + 60 batch4)
- `transform_expr` refactored from by-value to `&mut Expr` for reduced AST cloning
- Stack overflow protection: sqlparser depth-50 cap, interposer stack measurement, worker thread delegation at 400KB

### PG modules (Phase 2) — hybrid C/Rust

All 7 backend modules now have their core logic implemented in Rust with thin C shims for FFI:

| Module | Description |
|--------|-------------|
| `pg_config` | Environment variable parsing, connection config |
| `pg_logging` | Log file writer with level filtering and throttling |
| `pg_mem_telemetry` | Opt-in allocation counters |
| `shim_alloc` | Lock-free allocation tracker |
| `pg_query_cache` | Thread-local translation cache |
| `pg_statement` | Prepared statement registry and cache |
| `pg_client` | Connection pool management |

~550 C tests across 25 suites continue to pass.

### Log level cleanup

5 informational `LOG_ERROR` messages demoted to `LOG_INFO`:
- Pool initialization messages
- Fresh connection succeeded messages

These are now filtered at the default `PLEX_PG_LOG_LEVEL=ERROR` setting, reducing log noise in production.

### Git repository cleanup

Removed accidentally committed `rust/sql-translator/target/` from all git history. Repository size reduced from 243MB to 60MB.

## Test counts

- **Rust:** 525 tests (318 lib + 54 batch1 + 51 batch2 + 42 batch3 + 60 batch4)
- **C:** ~550 tests across 25 suites
- **Total:** 1,075+ tests

## Upgrading

Drop-in replacement. No configuration changes needed.
