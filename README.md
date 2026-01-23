# plex-postgresql

[![en](https://img.shields.io/badge/lang-en-red.svg)](README.md)
[![es](https://img.shields.io/badge/lang-es-yellow.svg)](README.es.md)

**Run Plex Media Server with PostgreSQL instead of SQLite.**

A shim library that intercepts Plex's SQLite calls and redirects them to PostgreSQL. Zero Plex modifications required.

## 🎉 Latest Release: v0.9.7

**Bug fix:** playQueues 500 errors resolved.

- ✅ **Fixed:** playQueues HTTP 500 errors (fallback to SQLite for edge cases)
- ✅ **Fixed:** Consistent 60s statement_timeout after connection reset
- ✅ **Improved:** Count query detection cached at prepare time for better performance
- ✅ **Refactored:** Platform code now shares common modules (36% code reduction)

[📥 Download v0.9.7](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.7) | [📋 Full Release Notes](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.7)

**Available for:** macOS ARM64 • Linux x86_64 • Linux ARM64 • Docker (multi-arch)

### Quick Install

**macOS:**
```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.7/db_interpose_pg.dylib \
  -o /usr/local/lib/db_interpose_pg.dylib
# Then configure DYLD_INSERT_LIBRARIES in Plex launchd plist
```

**Linux (x86_64):**
```bash
sudo curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.7/db_interpose_pg_linux_x86_64.so \
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

## Benchmark Results

### Concurrent Access (The Real Problem)

Real-world test: **Plex + Kometa + PMM + 4 concurrent streams** (7 separate processes, 15 seconds):

| Metric | SQLite | PostgreSQL (TCP) | PostgreSQL (Socket) |
|--------|--------|------------------|---------------------|
| Total Writes | 727,330 | 25,851 | 27,543 |
| **Write Errors** | **592,664 (81%)** | **0** | **0** |
| Total Reads | 5,173 | 1,115 | 1,121 |
| Read Errors | 0 | 0 | 0 |

**What this means:**
- SQLite: 81% of writes fail due to database locking
- SQLite: ~2.4 million errors per minute under load
- PostgreSQL: Zero errors, everything works simultaneously
- Unix socket: ~6% faster than TCP (negligible for most setups)

### Query Latency Comparison

| Query Type | SQLite | PostgreSQL (Socket) | Overhead |
|------------|--------|---------------------|----------|
| SELECT (PK lookup) | 3.9 µs | 18.2 µs | 4.6x |
| INSERT (batched) | 0.7 µs | 15.5 µs | 22x |
| Range Query | 22.0 µs | 45.2 µs | 2.1x |

PostgreSQL is slower per-query, but **never locks**. For Plex + rclone/Real-Debrid, smooth playback matters more than raw speed.

### Shim Overhead

| Component | Latency | Throughput |
|-----------|---------|------------|
| SQL Translation (uncached) | 17.5 µs | 57K/sec |
| **SQL Translation (cached)** | **0.12 µs** | **8.5M/sec** |
| Cache Lookup | 22.6 ns | 354M/sec |

The thread-local translation cache provides **145x speedup** for repeated queries. Shim overhead is **<1% of total query time**.

### Run Benchmarks

```bash
# Multi-process stress test (the definitive proof)
PLEX_PG_SOCKET=/tmp python3 scripts/benchmark_multiprocess.py

# SQLite vs PostgreSQL latency comparison
python3 tests/bench_sqlite_vs_pg.py

# Shim component micro-benchmarks
make benchmark

# Cache implementation comparison (mutex vs thread-local)
./tests/bin/bench_cache
```

For rclone/Real-Debrid setups with Kometa/PMM, **SQLite becomes unusable** during library scans. PostgreSQL handles it without issues.

## What's New in v0.9.7

### Bug Fix: playQueues 500 Error

**Problem:** `/playQueues` endpoint returned HTTP 500 errors for certain edge cases.

**Root Cause:** Some queries resulted in PostgreSQL statements without bound parameters (paramless pg_stmt), which caused issues during execution.

**Solution:** Fallback to SQLite for these edge cases, ensuring all playQueues requests succeed.

### Performance Improvements

- **Count query detection** cached at prepare time (was: per-step)
- **Consistent statement_timeout** (60s) after connection reset

### Code Quality

- **36% code reduction** through platform code refactoring
- Shared common module (`db_interpose_common.c`) for macOS and Linux
- Platform-specific backtrace implementations abstracted via `platform_print_backtrace()`
- Common signal handler and exception logging

**See also:** [v0.9.2 Release Notes](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.2) for timeline 500 fix details.

### Easy Installation

New interactive installer script:
```bash
./install.sh  # One command, handles everything
```

Features:
- Automatic Plex binary backup
- Creates start/stop scripts
- Generates uninstall script
- Architecture and OS validation

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
- ✅ v0.9.7 fixes active (playQueues + timeline errors fixed)
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
- ✅ Tested: 34 tables, 89K+ items migrated successfully
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

**Latest Release:** [v0.9.7](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.7) - Fixes playQueues + timeline errors

```bash
# Download the shim
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.7/db_interpose_pg.dylib \
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

**Latest Release:** [v0.9.7](https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.7) - Fixes playQueues + timeline errors

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
sudo curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.7/db_interpose_pg_linux_x86_64.so \
  -o /usr/local/lib/db_interpose_pg.so
sudo chmod 644 /usr/local/lib/db_interpose_pg.so
```

**For ARM64 (aarch64):**
```bash
sudo curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.7/db_interpose_pg_linux_arm64.so \
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
# "PostgreSQL shim initialized (v0.9.7)"

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

To migrate an existing Plex library to PostgreSQL:

```bash
# macOS
./scripts/migrate_sqlite_to_pg.sh

# Docker (mount source database first)
docker exec plex-postgresql bash -c '
  export PGHOST=postgres PGUSER=plex PGPASSWORD=plex PGDATABASE=plex
  SQLITE_DB="/source-db/com.plexapp.plugins.library.db"

  for TABLE in $(sqlite3 "$SQLITE_DB" ".tables"); do
    COLS=$(sqlite3 "$SQLITE_DB" "PRAGMA table_info($TABLE);" | cut -d"|" -f2 | tr "\n" ",")
    sqlite3 -header -csv "$SQLITE_DB" "SELECT * FROM $TABLE;" > /tmp/$TABLE.csv
    psql -c "\\copy plex.$TABLE($COLS) FROM /tmp/$TABLE.csv WITH CSV HEADER"
  done
'
```

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

```
macOS:  Plex → SQLite API → DYLD_INTERPOSE shim → SQL Translator → PostgreSQL
Linux:  Plex → SQLite API → LD_PRELOAD shim    → SQL Translator → PostgreSQL
Docker: Plex → SQLite API → LD_PRELOAD shim    → SQL Translator → PostgreSQL (container)
```

The shim intercepts all `sqlite3_*` calls, translates SQL syntax (placeholders, functions, types), and executes on PostgreSQL via libpq.

### Architecture

The codebase uses a modular architecture with platform-specific cores and shared common code:

```
src/
├── db_interpose_common.c      # Shared: function pointers, exception handling, fork handlers
├── db_interpose_common.h      # Common declarations
├── db_interpose_core.c        # macOS: fishhook + execinfo.h backtrace (368 lines)
├── db_interpose_core_linux.c  # Linux: LD_PRELOAD + /proc/maps backtrace (646 lines)
├── db_interpose_*.c           # Shared: open, exec, prepare, bind, step, column, metadata
├── sql_translator.c           # SQLite → PostgreSQL SQL translation
├── sql_tr_*.c                 # Translation modules: functions, types, quotes, etc.
└── pg_*.c                     # PostgreSQL client, connection pool, statement cache
```

**Code sharing:** Platform-specific code reduced by 36% (2889 → 1844 lines) through extraction of common modules.

### Key Features

- **Connection pooling** - Efficient reuse of PostgreSQL connections
- **SQL translation** - Automatic SQLite → PostgreSQL syntax conversion
- **Prepared statements** - Query caching for performance
- **Schema initialization** - Auto-creates PostgreSQL schema on first run
- **Circular reference protection** - Trigger prevents self-referential parent_id crashes
- **Stack overflow protection** - Multi-layer defense against crashes (see below)
- **Auto-build** - Wrapper automatically rebuilds shim if dylib is missing

### SQL Translation Features

The translator handles SQLite-specific syntax automatically:

| SQLite | PostgreSQL |
|--------|------------|
| `COLLATE NOCASE` | `LOWER()` comparisons |
| `WHERE column LIKE '%x%' COLLATE NOCASE` | `WHERE column ILIKE '%x%'` |
| `WHERE 0` / `WHERE 1` | `WHERE FALSE` / `WHERE TRUE` |
| `iif(cond, a, b)` | `CASE WHEN cond THEN a ELSE b END` |
| `strftime('%s', x)` | `EXTRACT(EPOCH FROM x)::bigint` |
| `IFNULL(a, b)` | `COALESCE(a, b)` |
| `title MATCH 'action -comedy'` | FTS with `!` negation |
| `title MATCH 'term1 AND term2'` | FTS with `&` operator |
| `title MATCH '"exact phrase"'` | FTS with `<->` adjacency |
| `?` placeholders | `$1, $2, ...` numbered params |

### Stack Protection

Plex uses small thread stacks (544KB) which can overflow during complex queries. The shim provides multi-layer protection:

| Layer | Threshold | Action |
|-------|-----------|--------|
| Worker delegation | < 400KB remaining | Delegate to 8MB worker thread |
| Hard protection (normal) | < 64KB remaining | Return SQLITE_NOMEM |
| Hard protection (worker) | < 32KB remaining | Return SQLITE_NOMEM |

This prevents stack overflow crashes that occurred with deep recursive queries (e.g., OnDeck with 218 recursive frames).

## Testing

Run unit tests to validate the shim:

```bash
# All unit tests (87 tests total)
make unit-test

# Individual test suites
make test-recursion      # Recursion guards, loop detection (11 tests)
make test-crash          # Production crash scenarios (21 tests)
make test-sql            # SQL translation (32 tests)
make test-cache          # Query cache logic (16 tests)
make test-tls            # Thread-local storage (7 tests)

# Benchmarks
make benchmark           # Shim component micro-benchmarks
```

### Benchmarks

Compare SQLite vs PostgreSQL (TCP and Unix socket) performance:

```bash
# Multi-process stress test (the definitive proof)
PLEX_PG_SOCKET=/tmp python3 scripts/benchmark_multiprocess.py

# Library scan + playback simulation  
PLEX_PG_SOCKET=/tmp python3 scripts/benchmark_plex_stress.py

# Concurrent writers test
PLEX_PG_SOCKET=/tmp python3 scripts/benchmark_locking.py

# Query performance comparison
python3 scripts/benchmark_compare.py

# Bash benchmark (use --socket for Unix socket mode)
./scripts/benchmark.sh           # TCP mode
./scripts/benchmark.sh --socket  # Unix socket mode
```

The stack protection test validates all protection layers by simulating low-stack conditions without running Plex.

## Known Issues

### ✅ FIXED in v0.9.2: Timeline 500 Error

**Status:** Fixed in v0.9.2  
**Issue:** `/:/timeline` endpoint returned HTTP 500 errors during playback with `std::exception`  
**Root Cause:** `sqlite3_last_insert_rowid()` called with different database handle, causing NULL connection lookup  
**Solution:** Fallback connection lookup + sequence advancement before skipping empty INSERTs  
**Action:** Update to v0.9.2 or later

See [What's New](#whats-new-in-v092) for details.

### ✅ FIXED in v0.8.12: TV Shows HTTP 500 Error

**Status:** Fixed in v0.8.12  
**Issue:** TV shows endpoint returned HTTP 500 with `std::bad_cast` exceptions  
**Root Cause:** Plex's SOCI library bug with BIGINT aggregate functions (count, sum, etc.)  
**Solution:** Aggregate functions declare as TEXT type to bypass SOCI's strict integer type checking  
**Impact:** TV shows now load correctly, MetadataCounterCache rebuilds work

## Known Limitations

### PostgreSQL Type Mapping

The shim translates SQLite types to PostgreSQL equivalents:

- **INTEGER** → INT4 (32-bit) or INT8 (64-bit based on context)
- **BIGINT** → INT8 (64-bit) - ✅ Fixed in v0.8.12
- **Aggregate functions** (count, sum, max, min, avg) → Declared as TEXT with 64-bit values
  - **Why TEXT?** Workaround for SOCI Issue #1190 - forces SOCI to use text-to-integer conversion which works correctly
  - **Impact:** None - values are still 64-bit integers, just declared differently to SOCI

### SOCI Type System Workaround

**Background:** Plex uses SOCI ORM which has a bug (SOCI Issue #1190) parsing BIGINT values from aggregate functions.

**Our solution (v0.8.12+):**
- Aggregate functions declare as TEXT type to SOCI
- Data is still 64-bit integers from PostgreSQL
- SOCI's text-to-int conversion works correctly
- Bypasses SOCI's buggy native BIGINT handling

**Impact:** Transparent to Plex - all functionality works correctly.

## Troubleshooting

```bash
# Check PostgreSQL
pg_isready -h localhost -U plex

# Check logs (macOS)
tail -50 /tmp/plex_redirect_pg.log

# Check logs (Docker)
docker-compose logs -f plex

# Analyze fallbacks
./scripts/analyze_fallbacks.sh
```

### Common Issues

**Plex won't start**: Check if PostgreSQL is running and accessible.

**Database errors**: Ensure the schema exists: `psql -U plex -d plex -c "CREATE SCHEMA IF NOT EXISTS plex;"`

**Docker port conflict**: Change port in `docker-compose.yml` if 8080 is in use.

## License

MIT - See [LICENSE](LICENSE)

---
*Unofficial project, not affiliated with Plex Inc. Use at your own risk.*
