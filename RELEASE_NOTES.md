# Release Notes - v0.9.38

**Release Date:** February 22, 2026

Exec retry: `sqlite3_exec` now has the same connection recovery and retry logic that `sqlite3_step` already had (Issue #8).

## What changed

`sqlite3_exec` previously had no pre-flight connection check and no retry wrapper. If PostgreSQL went down during an exec call, the error was silently swallowed and the caller got `SQLITE_OK` back.

Now:
- Pre-flight `PQstatus` check before every exec query, with inline reconnect (PQreset, then fresh PQconnectdb if needed)
- Retry wrapper with configurable backoff from `PLEX_PG_RETRY_DELAYS` (default: 500, 1000, 2000, 3000, 4000ms)
- Connection errors return `SQLITE_ERROR` instead of being silently ignored

Same pattern as the step retry wrapper added in v0.9.34.

## Upgrading

Drop-in replacement. No configuration changes needed.
