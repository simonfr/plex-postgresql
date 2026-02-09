# Code Organization

## Project Structure

```
plex-postgresql/
├── src/
│   ├── db_interpose_core.c       macOS: DYLD_INTERPOSE + fishhook initialization
│   ├── db_interpose_core_linux.c Linux: LD_PRELOAD + dlsym(RTLD_NEXT)
│   ├── db_interpose_common.c/h   Shared: exception tracking, fork handlers, signal handling
│   ├── db_interpose_open.c       sqlite3_open/close interception
│   ├── db_interpose_prepare.c    sqlite3_prepare_v2 interception
│   ├── db_interpose_bind.c       sqlite3_bind_* interception
│   ├── db_interpose_step.c       sqlite3_step interception
│   ├── db_interpose_column.c     sqlite3_column_* interception
│   ├── db_interpose_metadata.c   sqlite3_column_name/count/decltype
│   ├── db_interpose_exec.c       sqlite3_exec interception
│   ├── db_interpose.h            Interpose declarations and macros
│   ├── pg_types.h                Core type definitions
│   ├── pg_config.c/h             Configuration loading + SQL classification
│   ├── pg_logging.c/h            Thread-safe logging infrastructure
│   ├── pg_client.c/h             Connection pool management + auto-reconnect
│   ├── pg_statement.c/h          Statement lifecycle, reference counting
│   ├── pg_query_cache.c/h        Query result caching (thread-local, TTL-based)
│   ├── sql_translator.c          SQL translation orchestrator + TLS cache
│   ├── sql_translator_internal.h Internal translator interfaces
│   ├── sql_tr_helpers.c          String utilities (strdup, replace, etc.)
│   ├── sql_tr_placeholders.c     ? → $1 placeholder translation
│   ├── sql_tr_functions.c        Function translations (iif, strftime, etc.)
│   ├── sql_tr_query.c            Query structure fixes (ORDER BY, LIMIT, CASE booleans)
│   ├── sql_tr_groupby.c          GROUP BY strict mode rewriting + NULLS FIRST
│   ├── sql_tr_types.c            Type translations (BLOB→BYTEA, DDL types)
│   ├── sql_tr_quotes.c           Quote translations (backticks, brackets)
│   ├── sql_tr_keywords.c         Keyword translations (GLOB, COLLATE NOCASE)
│   ├── sql_tr_upsert.c           UPSERT/ON CONFLICT handling (28 table mappings)
│   └── fishhook.c                macOS runtime symbol rebinding
├── include/
│   ├── sql_translator.h          Translator public interface
│   └── fishhook.h                fishhook public interface
├── scripts/
│   ├── install_wrappers.sh       Install Plex wrappers (macOS)
│   ├── install_wrappers_linux.sh Install Plex wrappers (Linux)
│   ├── uninstall_wrappers.sh     Restore original binaries (macOS)
│   ├── uninstall_wrappers_linux.sh Restore original binaries (Linux)
│   ├── migrate_sqlite_to_pg.sh   SQLite → PostgreSQL migration
│   ├── migrate_pg_to_sqlite.sh   PostgreSQL → SQLite migration (rollback)
│   ├── migrate_lib.sh            Shared migration library functions
│   ├── doctor.sh                 Diagnostic health check for installations
│   ├── docker-entrypoint.sh      Docker container entrypoint
│   ├── standalone-entrypoint.sh  Standalone Docker entrypoint
│   ├── analyze_fallbacks.sh      Analyze fallback queries (passed to SQLite)
│   ├── benchmark.sh              PostgreSQL raw benchmark
│   ├── benchmark_compare.sh      Shell-based SQLite vs PostgreSQL comparison
│   ├── benchmark_compare.py      Python SQLite vs PostgreSQL comparison
│   ├── benchmark_plex_stress.py  Library scan + playback simulation
│   ├── benchmark_multiprocess.py Multi-process concurrent access test
│   └── benchmark_locking.py      Database locking contention test
├── tests/
│   ├── src/                      Unit test sources (25 files, see below)
│   ├── test_group_by_rewriter.c  GROUP BY rewriter test (31 tests)
│   ├── bench_cache.c             Cache implementation benchmark
│   ├── bench_translation.c       Translation pipeline benchmark
│   ├── bench_shim.c              Full shim benchmark
│   ├── bench_pipeline.c          Pipeline stage benchmark
│   ├── bench_micro.c             Micro-operation benchmark
│   ├── bench_libpq.c             Raw libpq benchmark
│   └── bench_sqlite_vs_pg.py     SQLite vs PostgreSQL latency comparison
├── schema/
│   ├── plex_schema.sql           PostgreSQL schema for Plex tables
│   ├── sqlite_schema.sql         Reference SQLite schema
│   └── sqlite_column_types.sql   Column type mapping reference
├── .github/workflows/
│   ├── ci.yml                    Unit test CI (656 tests, Linux)
│   ├── docker-publish.yml        Docker image publishing
│   ├── release-linux-artifacts.yml  Linux release build (aarch64 + x86_64)
│   └── release-macos-artifacts.yml  macOS release build (universal binary)
└── docs/
    └── MODULES.md                This file
```

## Module Overview

### Core Interposition

#### db_interpose_core.c (macOS)
Entry point for macOS. Uses `DYLD_INTERPOSE` for static interposition and `fishhook` for runtime symbol rebinding to intercept SQLite calls from dynamically loaded libraries (like SOCI).

#### db_interpose_core_linux.c (Linux)
Entry point for Linux. Uses `LD_PRELOAD` with `dlsym(RTLD_NEXT)` to intercept SQLite calls.

#### db_interpose_common.c
Platform-independent shared code: exception type tracking (`__cxa_throw` interception), fork safety handlers (`pthread_atfork`), signal handlers for crash diagnostics, symbol verification, and common initialization/cleanup.

### Interception Modules

| Module | Functions Intercepted |
|--------|----------------------|
| `db_interpose_open.c` | `sqlite3_open`, `sqlite3_open_v2`, `sqlite3_close`, `sqlite3_close_v2` |
| `db_interpose_prepare.c` | `sqlite3_prepare_v2`, `sqlite3_prepare16_v2`, `sqlite3_finalize` |
| `db_interpose_bind.c` | `sqlite3_bind_*` (int, int64, double, text, blob, null), `sqlite3_clear_bindings`, `sqlite3_bind_parameter_index` |
| `db_interpose_step.c` | `sqlite3_step`, `sqlite3_reset` |
| `db_interpose_column.c` | `sqlite3_column_*` (int, int64, double, text, blob, bytes, type), `sqlite3_column_value` |
| `db_interpose_metadata.c` | `sqlite3_column_name`, `sqlite3_column_count`, `sqlite3_column_decltype`, `sqlite3_changes`, `sqlite3_last_insert_rowid` |
| `db_interpose_exec.c` | `sqlite3_exec` |

### Translation Modules

| Module | Responsibility |
|--------|---------------|
| `sql_translator.c` | Main orchestrator, thread-local cache management |
| `sql_tr_helpers.c` | String utilities (strdup, replace, etc.) |
| `sql_tr_placeholders.c` | `?` → `$1`, `:name` → `$2` |
| `sql_tr_functions.c` | `iif` → `CASE`, `strftime` → `EXTRACT`, `IFNULL` → `COALESCE`, `typeof` → `pg_typeof`, `unixepoch`, `json_each` |
| `sql_tr_query.c` | Query structure fixes (ORDER BY, LIMIT -1, forward-ref joins, CASE boolean 0/1 → FALSE/TRUE) |
| `sql_tr_groupby.c` | GROUP BY strict mode rewriting (PostgreSQL requires all non-aggregate columns), NULLS FIRST ordering |
| `sql_tr_types.c` | `BLOB` → `BYTEA`, DDL type translation (`datetime`, `integer`, etc.), `sqlite_master` → `pg_catalog` |
| `sql_tr_quotes.c` | Backticks → double quotes, bracket quotes → double quotes |
| `sql_tr_keywords.c` | `GLOB` → `LIKE`, `COLLATE NOCASE` → `ILIKE`, operator spacing |
| `sql_tr_upsert.c` | `INSERT OR REPLACE` → `ON CONFLICT DO UPDATE` with 28 table-specific conflict target mappings, special column handling (updated_at COALESCE, view_count GREATEST) |

### PostgreSQL Client

| Module | Responsibility |
|--------|---------------|
| `pg_client.c` | Connection pool (50 connections default, max 100), auto-reconnect on failure |
| `pg_statement.c` | Statement lifecycle, reference counting, metadata settings upsert |
| `pg_query_cache.c` | Query result caching (thread-local, TTL-based eviction) |
| `pg_config.c` | Environment variable configuration, SQL classification (should_redirect, is_write/read_operation) |
| `pg_logging.c` | Thread-safe logging, deadlock prevention |

## Caching Architecture

### Three-Layer Cache System

```
┌─────────────────────────────────────────────────────────────────┐
│                     Plex Query                                   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Layer 1: SQL Translation Cache (Thread-Local)                  │
│  ─────────────────────────────────────────────────────────────  │
│  • 512 entries per thread                                       │
│  • Lock-free (no mutex contention)                              │
│  • FNV-1a hash with linear probing                              │
│  • Hit: 22.6 ns → Miss: 17.5 µs (775x speedup)                  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Layer 2: Prepared Statement Cache                              │
│  ─────────────────────────────────────────────────────────────  │
│  • PostgreSQL server-side prepared statements                   │
│  • Avoids re-parsing SQL on PostgreSQL                          │
│  • Automatic per-connection                                     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Layer 3: Query Result Cache (Thread-Local)                     │
│  ─────────────────────────────────────────────────────────────  │
│  • Caches SELECT results for identical queries                  │
│  • TTL-based eviction (configurable)                            │
│  • Hit rate tracking for statistics                             │
└─────────────────────────────────────────────────────────────────┘
```

### Cache Performance

| Cache Type | Hit Latency | Miss Latency | Speedup |
|------------|-------------|--------------|---------|
| Translation (TLS) | 22.6 ns | 17.5 µs | **775x** |
| Query Result | ~1 ns (hash) | ~20 µs (PG query) | **20,000x** |

### Why Thread-Local?

Benchmark of different cache implementations (8 threads, 1M ops/thread):

| Implementation | Latency | Throughput | Notes |
|----------------|---------|------------|-------|
| Mutex | 507 ns | 15.8 M/sec | Global lock contention |
| RWLock | 2,246 ns | 3.6 M/sec | Worse than mutex (!) |
| **Thread-Local** | **22.6 ns** | **354 M/sec** | No contention |
| Lock-Free | 22.9 ns | 350 M/sec | Similar to TLS |

Thread-local storage is **22x faster** than mutex-protected global cache.

## Execution Flow

### Query Execution

```
Plex App
    │
    ▼
sqlite3_prepare_v2(sql)
    │
    ├─ Check: Is this a PostgreSQL table?
    │     No  → Pass to real SQLite (shadow DB)
    │     Yes ↓
    │
    ├─ Translation Cache Lookup (22.6 ns)
    │     Hit  → Use cached PostgreSQL SQL
    │     Miss → sql_translate() (17.5 µs) → Cache result
    │
    ├─ Create pg_stmt_t with translated SQL
    │
    ▼
sqlite3_bind_*(stmt, ...)
    │
    ├─ Store parameters in pg_stmt_t
    │
    ▼
sqlite3_step(stmt)
    │
    ├─ Query Result Cache Lookup
    │     Hit  → Return cached PGresult
    │     Miss ↓
    │
    ├─ PQexecPrepared() → PostgreSQL
    │
    ├─ Cache result if cacheable
    │
    ▼
sqlite3_column_*(stmt, idx)
    │
    ├─ PQgetvalue(result, row, idx)
    │
    ├─ Convert to SQLite type
    │
    ▼
Result → Plex App
```

### Connection Pool Flow

```
sqlite3_open_v2("plex.db")
    │
    ├─ pg_pool_get_connection()
    │     ├─ Check TLS cached connection (fast path, 99% of calls)
    │     ├─ If miss: Find free slot in pool
    │     ├─ If no free: Create new connection (up to pool_size)
    │     └─ Return pg_connection_t*
    │
    ▼
... execute queries ...
    │
    ▼
sqlite3_close(db)
    │
    ├─ pg_pool_release_connection()
    │     └─ Mark slot as available (don't close actual connection)
    │
    ▼
Connection stays in pool for reuse
```

## SQL Translation Pipeline

```
SQLite SQL
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  1. Schema Prefix                                         │
│     metadata_items → plex.metadata_items                  │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  2. Placeholder Translation                               │
│     ? → $1, $2, $3...                                     │
│     :name → $N (with mapping table)                       │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  3. Function Translation                                  │
│     iif(a,b,c) → CASE WHEN a THEN b ELSE c END           │
│     strftime('%s',x) → EXTRACT(EPOCH FROM x)::bigint     │
│     IFNULL(a,b) → COALESCE(a,b)                          │
│     datetime('now') → NOW()                               │
│     SUBSTR(a,b,c) → SUBSTRING(a FROM b FOR c)            │
│     INSTR(a,b) → POSITION(b IN a)                        │
│     typeof(x) → pg_typeof(x)::text                       │
│     unixepoch('now') → EXTRACT(EPOCH FROM NOW())::bigint  │
│     json_each(x).value → jsonb_array_elements_text(x)    │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  4. Type Translation                                      │
│     BLOB → BYTEA                                          │
│     INTEGER PRIMARY KEY → SERIAL (on CREATE)              │
│     DDL: datetime, float, boolean, varchar → PG types     │
│     sqlite_master → pg_catalog.pg_tables                  │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  5. Query Structure                                       │
│     CASE WHEN x THEN 1 ELSE 0 END → boolean              │
│     WHERE 0 / WHERE 1 → WHERE FALSE / WHERE TRUE          │
│     LIMIT -1 → (removed)                                  │
│     Forward-reference joins → reordered                   │
│     GROUP BY → add missing non-aggregate columns          │
│     ORDER BY → add NULLS FIRST where needed               │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  6. Keyword Translation                                   │
│     GLOB '*term*' → LIKE '%term%'                         │
│     COLLATE NOCASE → ILIKE / LOWER()                      │
│     Operator spacing normalization                        │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  7. UPSERT Translation                                    │
│     INSERT OR REPLACE → INSERT ... ON CONFLICT DO UPDATE  │
│     INSERT OR IGNORE → INSERT ... ON CONFLICT DO NOTHING  │
│     28 table-specific conflict target mappings            │
│     Special: COALESCE(updated_at), GREATEST(view_count)   │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  8. Quote Translation                                     │
│     `column` → "column"                                   │
│     [column] → "column"                                   │
└──────────────────────────────────────────────────────────┘
    │
    ▼
PostgreSQL SQL
```

## Testing

### Test Suites

698 tests across 22 suites. CI runs 656 tests (18 suites); 3 suites (test-api, test-expanded, test-params) require the shim loaded via LD_PRELOAD and 1 suite (test-stack-macos) is macOS-only.

| Makefile Target | Test File | Tests | Description |
|-----------------|-----------|-------|-------------|
| `test-recursion` | `test_recursion.c` | 17 | Recursion guards, stack overflow protection |
| `test-crash` | `test_crash_scenarios.c` | 27 | Crash scenarios from production history |
| `test-sql` | `test_sql_translator.c` | 198 | Full SQL translation pipeline (functions, types, keywords, placeholders, CASE booleans, GROUP BY, etc.) |
| `test-groupby` | `test_group_by_rewriter.c` | 31 | GROUP BY strict mode rewriting + NULLS FIRST ordering |
| `test-upsert` | `test_upsert.c` | 59 | UPSERT: all 28 conflict targets, schema prefix stripping, special columns, quoted columns |
| `test-types` | `test_type_normalization.c` | 42 | decltype normalization for SOCI compatibility |
| `test-soci` | `test_decltype_soci_compat.c` | 41 | SOCI std::bad_cast prevention (type coercion) |
| `test-cache` | `test_query_cache.c` | 25 | Query result cache (TTL, eviction, hash collisions) |
| `test-tls` | `test_tls_cache.c` | 7 | Thread-local storage cache correctness |
| `test-fork` | `test_fork_safety.c` | 9 | pthread_atfork handler correctness |
| `test-reaper` | `test_pool_reaper.c` | 12 | Connection pool idle reaper |
| `test-buffer` | `test_buffer_pool.c` | 14 | column_text buffer expansion |
| `test-logging` | `test_logging_deadlock.c` | 9 | Logging deadlock prevention (10s timeout) |
| `test-exception` | `test_exception_handler.c` | 17 | C++ exception interception (__cxa_throw, __cxa_demangle) |
| `test-fts` | `test_fts_quotes.c` | 10 | FTS escaped quote handling |
| `test-config` | `test_pg_config.c` | 63 | SQL classification (should_redirect, should_skip, is_write/read) |
| `test-bind` | `test_bind_helpers.c` | 27 | Binary detection, hex encoding (contains_binary_bytes, bytes_to_pg_hex) |
| `test-common` | `test_common_helpers.c` | 25 | is_library_db_path, simple_str_replace |
| `test-statement` | `test_statement_helpers.c` | 23 | metadata_settings upsert, metadata ID extraction |
| `test-api` | `test_sqlite_api.c` | — | SQLite API with shim loaded (requires LD_PRELOAD) |
| `test-expanded` | `test_expanded_sql.c` | — | sqlite3_expanded_sql + boolean conversion (requires LD_PRELOAD) |
| `test-params` | `test_bind_parameter_index.c` | — | Named parameter mapping (requires LD_PRELOAD) |
| `test-stack-macos` | `test_stack_macos.c` | — | macOS stack protection integration test |

### Running Tests

```bash
# All unit tests (698 tests, 22 suites — requires shim built + PostgreSQL)
make unit-test

# CI-safe subset (656 tests, 18 suites — no shim/PostgreSQL needed)
make ci-test

# Individual suites
make test-sql          # SQL translation (198 tests)
make test-upsert       # UPSERT handling (59 tests)
make test-config       # SQL classification (63 tests)
make test-types        # Type normalization (42 tests)
make test-soci         # SOCI compatibility (41 tests)
make test-groupby      # GROUP BY rewriting (31 tests)
make test-crash        # Crash scenarios (27 tests)
make test-bind         # Bind helpers (27 tests)
make test-common       # Common helpers (25 tests)
make test-cache        # Query cache (25 tests)
make test-statement    # Statement helpers (23 tests)
make test-recursion    # Recursion/stack (17 tests)
make test-exception    # Exception handler (17 tests)
make test-buffer       # Buffer pool (14 tests)
make test-reaper       # Pool reaper (12 tests)
make test-fts          # FTS quotes (10 tests)
make test-fork         # Fork safety (9 tests)
make test-logging      # Logging deadlock (9 tests)
make test-tls          # TLS cache (7 tests)

# Benchmarks
make benchmark                         # Shim micro-benchmarks
```

## Development

### Build

```bash
# macOS (auto-detect)
make clean && make

# Explicit macOS build
make macos

# Linux
make clean && make linux

# With debug symbols
make DEBUG=1
```

### Debug

```bash
export PLEX_PG_LOG_LEVEL=2  # Enable DEBUG logging
tail -f /tmp/plex_redirect_pg.log

# Analyze fallback queries (queries passed to SQLite)
./scripts/analyze_fallbacks.sh

# Health check
./scripts/doctor.sh
```

### Adding Function Translation

1. Add to `src/sql_tr_functions.c`:
```c
// In translate_functions()
result = replace_function(result, "new_func(", "pg_equivalent(");
```

2. Add test to `tests/src/test_sql_translator.c`

3. Rebuild: `make clean && make`

### Adding a New Intercepted Function

1. Declare in `src/db_interpose.h`:
```c
VISIBLE extern return_type (*orig_sqlite3_func)(args);
EXPORT return_type my_sqlite3_func(args);
```

2. Define in appropriate `db_interpose_*.c`:
```c
VISIBLE return_type (*orig_sqlite3_func)(args) = NULL;

return_type my_sqlite3_func(args) {
    LOG_DEBUG("FUNC: ...");
    pg_stmt_t *pg_stmt = pg_find_stmt(stmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // PostgreSQL path
    }
    // Fallback to original
    return orig_sqlite3_func ? orig_sqlite3_func(args) : default_value;
}
```

3. Add to rebindings array in `db_interpose_core.c`:
```c
{"sqlite3_func", my_sqlite3_func, (void**)&orig_sqlite3_func},
```

4. Add dlsym fallback:
```c
if (!orig_sqlite3_func) orig_sqlite3_func = dlsym(sqlite_handle, "sqlite3_func");
```
