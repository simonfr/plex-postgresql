# Release Notes - v0.9.39

**Release Date:** February 22, 2026

Stale prepared statement recovery after PostgreSQL restart.

## What changed

After a PostgreSQL restart, server-side prepared statements are gone but the shim's per-connection cache still referenced them. This caused queries to fail with "prepared statement does not exist" and retries to loop on the same stale cache entry until exhausted.

Now:
- Detects SQLSTATE `26000` (invalid_sql_statement_name) via `PQresultErrorField`
- Clears the local stmt cache without sending DEALLOCATE round-trips to the server
- Lets the existing retry wrapper re-prepare and re-execute the query
- All 6 error handlers in step.c and exec.c are covered

Previously: PG restart required a Plex restart to recover.
Now: PG restart recovers automatically within seconds.

## Upgrading

Drop-in replacement. No configuration changes needed.
