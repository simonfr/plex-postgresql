# Release Notes - v0.9.35

**Release Date:** February 21, 2026

Docker standalone fix: removes slow `chown -R` calls that caused massive startup delays on large libraries.

## Highlights

### Docker Standalone: chown -R Delay Fix (PR #7)

- **Problem:** `standalone-entrypoint.sh` ran `chown -R plex:plex` on the entire Plex data directory at startup. On multi-TB libraries this caused startup delays of minutes. Additionally, when `PLEX_UID`/`PLEX_GID` are set, this caused a triple-chown: our script set 1000:1000, then Plex corrected to the right uid/gid, requiring another chown to fix permissions.
- **Fix:** Removed both `chown -R plex:plex` calls. Plex already handles ownership itself via `40-plex-first-run`.

## Upgrading

No action required. Pull the latest Docker image or rebuild from source.
