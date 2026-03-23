# Translator Docs

This directory is the entry point for translator behavior, scope, and test coverage.

## Map
- Coverage ledger: `docs/TRANSLATOR_COVERAGE.md`
- Test tag taxonomy: `TRANSLATOR_TEST_TAGS.md`
- Supported SQLite subset: `docs/translator/SUPPORTED_SQLITE_SUBSET.md`
- Known gaps and intentional non-goals: `docs/translator/KNOWN_GAPS.md`

## Design Boundaries
- The target is practical SQLite compatibility for Plex workloads, not full SQLite emulation.
- The translator prefers AST-driven rewrites where the parser can represent the input cleanly.
- For unsupported or dangerous surface area, the code should either no-op explicitly, fail clearly, or log a known limitation.
- Optional PostgreSQL output validation is a guardrail, not the primary translation mechanism.

## Maintenance Rules
- Add or update a coverage entry when a new rewrite family is introduced.
- Tag new translator tests using the conventions in `TRANSLATOR_TEST_TAGS.md`.
- Record unsupported but observed SQLite patterns in `docs/translator/KNOWN_GAPS.md`.
- Keep the documented subset aligned with what is actually tested in `rust/plex-pg-core`.
