# plex-postgresql

[![en](https://img.shields.io/badge/lang-en-red.svg)](README.md)
[![es](https://img.shields.io/badge/lang-es-yellow.svg)](README.es.md)

**Run Plex Media Server with PostgreSQL instead of SQLite.**

A shim library that intercepts Plex's SQLite calls and redirects them to PostgreSQL. Zero Plex modifications required.

## 🎉 Latest Release: v0.9.15

**Release automation update:** Linux release binaries are now built and uploaded automatically by GitHub Actions on tag push.

- ✅ **Added:** CI workflow to build/upload Linux `x86_64` + `aarch64` artifacts per release tag
- ✅ **Fixed:** manual workflow runs now build from the requested tag ref
- ✅ **Fixed:** architecture-aware PostgreSQL builder flags for more stable CI release builds

[📥 Download v0.9.15](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.15) | [📋 Full Release Notes](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.15)

Linux release binaries are built by GitHub Actions on tag push via `.github/workflows/release-linux-artifacts.yml`.

**Available for:** macOS ARM64 • Linux x86_64 • Linux ARM64 • Docker (multi-arch)

### Quick Install

**macOS:**
```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.15/db_interpose_pg.dylib \
  -o /usr/local/lib/db_interpose_pg.dylib
# Then configure DYLD_INSERT_LIBRARIES in Plex launchd plist
```

**Linux (x86_64):**
```bash
sudo curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.15/db_interpose_pg-linux-x86_64.so \
  -o /usr/local/lib/db_interpose_pg.so
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

SQLite is great for most Plex installations, but has one major limitation: **database locking**.

- **No more locking** - SQLite locks the entire database during writes. Library scans block playback. Concurrent scans queue up. With PostgreSQL, everything runs simultaneously - scan your libraries while streaming without interruption.
- **Remote storage** - Better I/O patterns for rclone, Real-Debrid, or cloud storage setups.
- **Large libraries** - PostgreSQL's query optimizer handles 10K+ movies and 50K+ episodes efficiently.
- **Standard tooling** - pg_dump for backups, replication, any PostgreSQL client for debugging.

## Benchmarks

Under concurrent load (Plex + Kometa + PMM + 4 streams), **82% of SQLite writes fail**. PostgreSQL: **zero errors**.

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

**doctor.sh** checks your database for missing triggers, functions, and tables that may have been added since your initial migration, and fixes them automatically. It also detects and repairs bad data (self-referential parents, cross-section parents, orphan seasons). Data changes require confirmation unless you pass `--fix`.

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

The easiest way to run Plex with PostgreSQL - works on **all platforms** (Linux, macOS, Windows).

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
4. Done! Your libraries are stored in PostgreSQL

**What happens:**
- ✅ PostgreSQL schema auto-created (empty)
- ✅ v0.9.8 fixes active (blobs.db + TOCTOU race conditions fixed)
- ✅ Multi-arch support (x86_64 + ARM64)
- ✅ All directories pre-created (Plug-ins, Metadata, Cache)
- ✅ No crashes, stable operation

### Migration from Existing SQLite Database

To migrate your existing Plex library to PostgreSQL:

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

**Migration performs:**
- ✅ Automatic detection of SQLite database
- ✅ Full data migration (all tables, metadata, posters, etc.)
- ✅ **NEW in v0.9.8:** blobs.db migration (thumbnails, artwork) via hex encoding
- ✅ Tested: 34 tables, 89K+ items + 4,344 blobs migrated successfully
- ✅ Original SQLite database remains unchanged (read-only mount)
- ✅ Automatic sequence updates
- ✅ Progress reporting per table

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

### Option 1: Pre-compiled Binary (Recommended)

**Latest Release:** [v0.9.10](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.10) - Kernel panic fix + SOCI NULL handling

```bash
# Download the shim
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.10/db_interpose_pg.dylib \
  -o /usr/local/lib/db_interpose_pg.dylib

# Configure Plex environment
sudo nano /Library/LaunchDaemons/com.plexapp.plexmediaserver.plist
```

Add environment variables to the plist file:
```xml
<key>EnvironmentVariables</key>
<dict>
  <key>DYLD_INSERT_LIBRARIES</key>
  <string>/usr/local/lib/db_interpose_pg.dylib</string>
  <key>PLEX_PG_HOST</key>
  <string>localhost</string>
  <key>PLEX_PG_PORT</key>
  <string>5432</string>
  <key>PLEX_PG_DATABASE</key>
  <string>plex</string>
  <key>PLEX_PG_USER</key>
  <string>plex</string>
  <key>PLEX_PG_PASSWORD</key>
  <string>your_password</string>
  <key>PLEX_PG_SCHEMA</key>
  <string>plex</string>
</dict>
```

Restart Plex:
```bash
sudo launchctl unload /Library/LaunchDaemons/com.plexapp.plexmediaserver.plist
sudo launchctl load /Library/LaunchDaemons/com.plexapp.plexmediaserver.plist
```

**Requirements:**
- macOS 11+ (Big Sur or later)
- Apple Silicon (M1/M2/M3/M4)
- Plex Media Server 1.40+
- PostgreSQL 12+ server running
- Plex database already migrated to PostgreSQL

### Option 2: Build from Source

#### 1. Setup PostgreSQL

```bash
brew install postgresql@15
brew services start postgresql@15

createuser plex
createdb -O plex plex
psql -d plex -c "ALTER USER plex PASSWORD 'plex';"
psql -U plex -d plex -c "CREATE SCHEMA plex;"
```

#### 2. Build & Install

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make clean && make

# Stop Plex, install wrappers
pkill -x "Plex Media Server" 2>/dev/null
./scripts/install_wrappers.sh
```

#### 3. Start Plex

```bash
open "/Applications/Plex Media Server.app"
```

The shim is auto-injected. Check logs: `tail -f /tmp/plex_redirect_pg.log`

### Uninstall

```bash
pkill -x "Plex Media Server" 2>/dev/null
./scripts/uninstall_wrappers.sh
```

## Quick Start (Linux Native)

**Recommended for production Linux installations** - better performance than Docker.

### Option 1: Pre-compiled Binary (Recommended)

**Latest Release:** [v0.9.10](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.10) - Kernel panic fix + SOCI NULL handling

**Available architectures:**
- ✅ x86_64 (Intel/AMD 64-bit)
- ✅ ARM64 (aarch64, Raspberry Pi 4/5, ARM servers)

#### Step 1: Setup PostgreSQL

```bash
# Install PostgreSQL
sudo apt install postgresql-15  # Ubuntu/Debian
# or: sudo yum install postgresql15-server  # RHEL/CentOS

# Create database and user
sudo -u postgres createuser plex
sudo -u postgres createdb -O plex plex
sudo -u postgres psql -c "ALTER USER plex PASSWORD 'yourpassword';"
sudo -u postgres psql -d plex -c "CREATE SCHEMA IF NOT EXISTS plex; ALTER SCHEMA plex OWNER TO plex;"
```

#### Step 2: Download and Install Shim

**For x86_64 (Intel/AMD):**
```bash
sudo curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.10/db_interpose_pg_linux_x86_64.so \
  -o /usr/local/lib/db_interpose_pg.so
sudo chmod 644 /usr/local/lib/db_interpose_pg.so
```

**For ARM64 (aarch64):**
```bash
sudo curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.10/db_interpose_pg_linux_arm64.so \
  -o /usr/local/lib/db_interpose_pg.so
sudo chmod 644 /usr/local/lib/db_interpose_pg.so
```

#### Step 3: Configure Plex systemd Service

```bash
# Create systemd override directory
sudo mkdir -p /etc/systemd/system/plexmediaserver.service.d

# Create environment configuration
sudo nano /etc/systemd/system/plexmediaserver.service.d/postgresql.conf
```

Add the following content:
```ini
[Service]
Environment="LD_PRELOAD=/usr/local/lib/db_interpose_pg.so"
Environment="PLEX_PG_HOST=localhost"
Environment="PLEX_PG_PORT=5432"
Environment="PLEX_PG_DATABASE=plex"
Environment="PLEX_PG_USER=plex"
Environment="PLEX_PG_PASSWORD=yourpassword"
Environment="PLEX_PG_SCHEMA=plex"
Environment="PLEX_PG_LOG_LEVEL=INFO"
```

#### Step 4: Restart Plex

```bash
sudo systemctl daemon-reload
sudo systemctl restart plexmediaserver
```

#### Step 5: Verify Installation

```bash
# Check Plex logs for PostgreSQL connection
sudo journalctl -u plexmediaserver -n 100 | grep -i postgres

# Expected output:
# "PostgreSQL connection established to localhost:5432/plex"
# "PostgreSQL shim initialized (v0.9.10)"

# Verify shim is loaded
sudo cat /proc/$(pgrep -f "Plex Media Server")/maps | grep db_interpose_pg
# Should show: /usr/local/lib/db_interpose_pg.so
```

**Requirements:**
- Linux x86_64 or ARM64
- Plex Media Server 1.40+
- PostgreSQL 12+ server running
- glibc 2.28+ (Ubuntu 18.04+, Debian 10+, RHEL 8+)
- Root access (for systemd configuration)

### Option 2: Build from Source

```bash
# Install dependencies
sudo apt install build-essential libsqlite3-dev libpq-dev postgresql-15

# Setup PostgreSQL
sudo -u postgres createuser plex
sudo -u postgres createdb -O plex plex
sudo -u postgres psql -c "ALTER USER plex PASSWORD 'plex';"

# Build and install
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make linux
sudo make install

# Install wrappers (auto-migrates database)
sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh

# Configure and start
sudo nano /etc/default/plexmediaserver  # Add PLEX_PG_* variables
sudo systemctl start plexmediaserver
```

### Uninstall

```bash
sudo systemctl stop plexmediaserver
sudo ./scripts/uninstall_wrappers_linux.sh
```

## Migration from SQLite

To migrate an existing Plex library to PostgreSQL (includes blobs.db since v0.9.8):

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

The performance difference is minimal - the real benefit of PostgreSQL is zero locking, not connection speed.

## How It Works

The shim intercepts all `sqlite3_*` calls, translates SQL syntax, and executes on PostgreSQL via libpq. Architecture, SQL translation tables, stack protection details: **[wiki/How It Works](https://github.com/cgnl/plex-postgresql/wiki/How-It-Works)**

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
