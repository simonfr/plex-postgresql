# plex-postgresql

[![en](https://img.shields.io/badge/lang-en-red.svg)](README.md)
[![es](https://img.shields.io/badge/lang-es-yellow.svg)](README.es.md)

**Run Plex Media Server with PostgreSQL instead of SQLite.**

A small shim library that catches Plex SQLite calls and sends them to PostgreSQL. You do not need to change Plex source code.

## 🎉 Latest Release: v0.9.16

**Wrapper reliability update:** fixes macOS wrapper portability and scanner uninstall restore behavior.

- ✅ **Fixed:** removed hardcoded local paths from generated macOS server wrapper
- ✅ **Fixed:** SQLite shadow `schema_migrations` is now synced from PostgreSQL in wrapper init
- ✅ **Fixed:** scanner backup/restore flow for reliable uninstall behavior

[📥 Download v0.9.16](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.16) | [📋 Full Release Notes](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.16)

Linux and macOS release zips are built by GitHub Actions on tag push via `.github/workflows/release-linux-artifacts.yml` and `.github/workflows/release-macos-artifacts.yml`.
Pull requests and `main` pushes run `.github/workflows/ci.yml` (script validation + Linux amd64 build check).
Docker images are published to GHCR on release tags via `.github/workflows/docker-publish.yml`:
- `ghcr.io/cgnl/plex-postgresql-linuxserver`
- `ghcr.io/cgnl/plex-postgresql-plexinc`

**Available for:** macOS ARM64 • Linux x86_64 • Linux ARM64 • Docker (multi-arch)

### Quick Install

**macOS:**
```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.16/plex-postgresql-v0.9.16-macos.zip \
  -o /tmp/plex-postgresql-macos.zip
unzip -j /tmp/plex-postgresql-macos.zip db_interpose_pg.dylib -d /usr/local/lib
# Then configure DYLD_INSERT_LIBRARIES in Plex launchd plist
```

**Linux (x86_64):**
```bash
sudo curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.16/plex-postgresql-v0.9.16-linux.zip \
  -o /tmp/plex-postgresql-linux.zip
sudo unzip -j /tmp/plex-postgresql-linux.zip db_interpose_pg-linux-x86_64.so -d /usr/local/lib
sudo mv /usr/local/lib/db_interpose_pg-linux-x86_64.so /usr/local/lib/db_interpose_pg.so
# Then configure LD_PRELOAD in systemd service
```

**Docker:**
```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
docker-compose up -d
```

See detailed installation instructions below for each platform.

## Platform Support

| Platform | Architecture | Status |
|----------|-------------|---------|
| macOS | ARM64 (M1/M2/M3) | ✅ Production tested |
| Linux | x86_64 | ✅ Pre-compiled binary |
| Linux | ARM64 | ✅ Pre-compiled binary |
| Docker | x86_64 + ARM64 | ✅ Multi-arch support |

## Why PostgreSQL?

SQLite works well for many Plex setups, but it can lock the full database during writes.

- **Fewer locks:** scans and playback can run together more smoothly.
- **Better for remote media:** useful for rclone and similar cloud setups.
- **Better at scale:** handles large libraries more easily.
- **Better tools:** backups and checks are easier with standard PostgreSQL tools.

## Benchmarks

In one stress test (Plex + Kometa + PMM + 4 streams), **82% of SQLite writes failed**. PostgreSQL had **zero errors**.

| Metric | SQLite | PostgreSQL |
|--------|--------|------------|
| Write Errors (15s test) | 8,019,177 (82%) | **0** |
| Shim overhead (cached) | — | 0.11 µs (<1%) |

Full results and how to run them: **[wiki/Benchmarks](https://github.com/cgnl/plex-postgresql/wiki/Benchmarks)**

## Migration & Maintenance

```bash
./scripts/migrate_sqlite_to_pg.sh   # SQLite → PostgreSQL
./scripts/migrate_pg_to_sqlite.sh   # PostgreSQL → SQLite (beta)
./scripts/doctor.sh                  # Check and fix schema + data
```

`doctor.sh` checks for missing tables, functions, and triggers, then fixes them. It also finds common data issues (self-parent rows, cross-section parents, orphan seasons). Data fixes ask for confirmation unless you use `--fix`.

```
$ ./scripts/doctor.sh
=== plex-postgresql doctor ===

Tables:
  maintenance_control                        OK
Functions:
  prevent_self_referential_parent()          OK
  maybe_cleanup_statistics()                 MISSING → FIXED
Triggers:
  trg_clean_statistics_resources             MISSING → FIXED
Data:
  self-referential parent_id                 OK
  orphan seasons (no parent)                 2 rows
  Fix them? [y/N]: y
  fixing orphan seasons... 2 rows
```

Flags: `--check` (only report, don't fix anything), `--fix` (fix everything without asking).

## Quick Start (Docker)

This is the easiest setup. It works on **Linux, macOS, and Windows**.

### Fresh Installation (No Existing Plex Database)

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# Start Plex + PostgreSQL
docker-compose up -d

# Check logs
docker-compose logs -f plex
```

**Setup:**
1. Open http://localhost:8080/web
2. Claim your server with Plex account
3. Add libraries via web interface
4. Done. Your library data now lives in PostgreSQL.

**What happens:**
- ✅ PostgreSQL schema auto-created (empty)
- ✅ v0.9.8 fixes active (blobs.db + TOCTOU race conditions fixed)
- ✅ Multi-arch support (x86_64 + ARM64)
- ✅ All directories pre-created (Plug-ins, Metadata, Cache)
- ✅ No crashes, stable operation

### Migration from Existing SQLite Database

If you already have a Plex library in SQLite, do this:

1. **Edit `docker-compose.yml`**, uncomment and update the source database path:
   ```yaml
   volumes:
     - plex_config:/config
     - postgres_socket:/var/run/postgresql
     # Uncomment and edit this line:
     - "/path/to/your/Plex Media Server/Plug-in Support/Databases:/source-db:ro"
   ```

2. **Platform-specific paths:**
   - **macOS**: `"${HOME}/Library/Application Support/Plex Media Server/Plug-in Support/Databases:/source-db:ro"`
   - **Linux**: `"/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases:/source-db:ro"`
   - **Windows**: `"C:/Users/YourName/AppData/Local/Plex Media Server/Plug-in Support/Databases:/source-db:ro"`

3. **Start containers:**
   ```bash
   docker-compose up -d
   ```

4. **Monitor migration:**
   ```bash
   docker-compose logs -f plex | grep -E "migration|Migration"
   ```

**Migration includes:**
- ✅ Detect SQLite database automatically
- ✅ Move full library data (tables, metadata, posters)
- ✅ Move `blobs.db` data (thumbnails and artwork)
- ✅ Keep source SQLite unchanged (read-only mount)
- ✅ Update PostgreSQL sequences automatically
- ✅ Show progress per table

### Configuration

Default PostgreSQL connection (via Unix socket for best performance):
```yaml
environment:
  - PLEX_PG_HOST=/var/run/postgresql  # Unix socket (7% faster)
  - PLEX_PG_DATABASE=plex
  - PLEX_PG_USER=plex
  - PLEX_PG_PASSWORD=plex
  - PLEX_PG_SCHEMA=plex
  - PLEX_PG_POOL_SIZE=50
  - PLEX_PG_LOG_LEVEL=DEBUG  # 0=ERROR, 1=INFO, 2=DEBUG
```

To use TCP instead of Unix socket:
```yaml
environment:
  - PLEX_PG_HOST=postgres  # TCP connection
  - PLEX_PG_PORT=5432
```

Mount your media libraries:
```yaml
volumes:
  - /path/to/movies:/movies:ro
  - /path/to/tv:/tv:ro
```

## Quick Start (macOS)

Use the latest macOS zip and run the wrapper installer.

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.16/plex-postgresql-v0.9.16-macos.zip -o /tmp/plex-pg-macos.zip
mkdir -p /tmp/plex-pg-macos
cd /tmp/plex-pg-macos
unzip /tmp/plex-pg-macos.zip

pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
open "/Applications/Plex Media Server.app"
```

To uninstall:

```bash
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/uninstall_wrappers.sh
```

## Quick Start (Linux Native)

Use the latest Linux zip and install the binary for your CPU.

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.16/plex-postgresql-v0.9.16-linux.zip -o /tmp/plex-pg-linux.zip
mkdir -p /tmp/plex-pg-linux
cd /tmp/plex-pg-linux
unzip /tmp/plex-pg-linux.zip

sudo mkdir -p /usr/local/lib/plex-postgresql
if [ "$(uname -m)" = "x86_64" ]; then
  sudo install -m 755 db_interpose_pg-linux-x86_64.so /usr/local/lib/plex-postgresql/db_interpose_pg.so
else
  sudo install -m 755 db_interpose_pg-linux-aarch64.so /usr/local/lib/plex-postgresql/db_interpose_pg.so
fi

sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh
sudo systemctl start plexmediaserver
```

To uninstall:

```bash
sudo systemctl stop plexmediaserver
sudo ./scripts/uninstall_wrappers_linux.sh
```

For full steps (PostgreSQL setup, environment variables, troubleshooting), see `INSTALL.md`.

## Migration from SQLite

Use this command to migrate an existing Plex library to PostgreSQL:

```bash
# macOS / Linux
./scripts/migrate_sqlite_to_pg.sh

# The script migrates:
# - library.db (metadata, media items, tags, etc.)
# - blobs.db (thumbnails, artwork, posters)
```

**What gets migrated:**
- All 34+ tables from library.db
- All blobs from blobs.db (thumbnails, artwork) via hex encoding
- Sequences automatically updated
- No Python dependencies required

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `PLEX_PG_HOST` | localhost | PostgreSQL host (or socket directory like `/tmp`) |
| `PLEX_PG_PORT` | 5432 | PostgreSQL port |
| `PLEX_PG_DATABASE` | plex | Database name |
| `PLEX_PG_USER` | plex | Database user |
| `PLEX_PG_PASSWORD` | (empty) | Database password |
| `PLEX_PG_SCHEMA` | plex | Schema name |
| `PLEX_PG_POOL_SIZE` | 50 | Connection pool size (max 100) |
| `PLEX_PG_LOG_LEVEL` | 1 | 0=ERROR, 1=INFO, 2=DEBUG |

### Unix Socket vs TCP

For local PostgreSQL, Unix sockets are ~5-6% faster than TCP:

```bash
# Use Unix socket (recommended for local PostgreSQL)
export PLEX_PG_HOST=/tmp  # or /var/run/postgresql on Linux

# Use TCP (required for remote PostgreSQL)
export PLEX_PG_HOST=localhost
```

The speed difference is small. The main win is fewer database locks.

## How It Works

The shim catches `sqlite3_*` calls, rewrites SQLite SQL to PostgreSQL SQL, and runs it through libpq. More technical details are in **[wiki/How It Works](https://github.com/cgnl/plex-postgresql/wiki/How-It-Works)**.

## Testing

```bash
make unit-test       # All 87 unit tests
make benchmark       # Shim micro-benchmarks
```

## Troubleshooting

```bash
pg_isready -h localhost -U plex          # Check PostgreSQL
./scripts/doctor.sh                       # Check and fix schema + data
tail -50 /tmp/plex_redirect_pg.log       # Check logs (macOS)
docker-compose logs -f plex              # Check logs (Docker)
```

More: **[wiki/Troubleshooting](https://github.com/cgnl/plex-postgresql/wiki/Troubleshooting)**

## License

MIT - See [LICENSE](LICENSE)

---
*Unofficial project, not affiliated with Plex Inc. Use at your own risk.*
