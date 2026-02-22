# Release Notes - v0.9.40

**Release Date:** February 22, 2026

Duplicate prepared statement handling (SQLSTATE 42P05) and connection init cleanup.

## What changed

After a Plex restart (while PostgreSQL keeps running), the shim's fresh empty cache would try to `PQprepare` statements that already exist on the PG backend, causing SQLSTATE 42P05 errors. Previously handled with fragile `strstr(err, "already exists")` text matching (locale-dependent), and missing entirely in the exec and METADATA_DESCRIBE paths.

Now:
- `pg_is_duplicate_prepared_stmt()` checks SQLSTATE `42P05` via `PQresultErrorField` (locale-independent)
- Replaces `strstr(err, "already exists")` on all 5 PQprepare sites in step.c, exec.c, and column.c
- METADATA_DESCRIBE path checks local cache before calling PQprepare (avoids unnecessary round-trips)
- New connections run `DEALLOCATE ALL` to clean up orphaned statements from previous shim instances
- 21 new unit tests for stmt cache (hash, lookup, add, clear, SQLSTATE detection, eviction)

This is the mirror fix to v0.9.39 (SQLSTATE 26000 for missing statements after PG restart). Together they cover both directions of cache/server desync.

## Upgrading

Drop-in replacement. No configuration changes needed.
