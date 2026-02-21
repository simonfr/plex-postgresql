# Release Notes - v0.9.37

**Release Date:** February 21, 2026

Documentation-only release: `PLEX_PG_IDLE_TIMEOUT` env var added to all config examples (Dockerfile.standalone, INSTALL.md, Linux install wrapper).

## Changes

- `Dockerfile.standalone`: added `PLEX_PG_IDLE_TIMEOUT=300` env var
- `INSTALL.md`: added `PLEX_PG_IDLE_TIMEOUT` to Docker, macOS, Linux, Advanced, and Troubleshooting sections
- `scripts/install_wrappers_linux.sh`: added `PLEX_PG_IDLE_TIMEOUT` default to server wrapper

## Upgrading

No code changes. Just documentation. If you're already on v0.9.36, no action needed.
