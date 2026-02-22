# Migratieplan: C naar Rust — SQL Translator (Laag 1) + PG Modules (Laag 2)

**Status: COMPLEET** (22 februari 2026)

## Architectuur (eindresultaat)

```
+---------------------------------------------+
| Laag 4+3: C interposer (~9.400 regels)      |  Blijft C
|  fishhook, constructor, DYLD_INTERPOSE       |
|  db_interpose_{open,prepare,step,bind,...}    |
+---------------------------------------------+
| Laag 2: Rust PG modules (hybride C/Rust)  ✅ |  rust/sql-translator/src/
|  pool, statement, cache, config, logging     |  pg_*.rs, shim_alloc.rs
+---------------------------------------------+
| Laag 1: Rust sql-translator               ✅ |  rust/sql-translator/src/
|  sqlparser-rs AST transforms                 |  lib.rs, functions.rs, etc.
+---------------------------------------------+
```

## Totaaloverzicht (afgerond)

|                         | C was       | Rust resultaat          |
|-------------------------|-------------|-------------------------|
| Laag 1: SQL translator  | 5.354 r     | ~5.000 r, 525 tests     |
| Laag 2: PG modules      | 4.907 r     | ~3.500 r (hybride C/Rust)|
| Totaal                  | 10.261 r    | ~8.500 r, 1.075+ tests  |

---

## LAAG 1: SQL translator ✅ COMPLEET

### Fase 1.1 ✅ -- Port alle C-tests naar Rust

Port alle input/output-paren uit `test_sql_translator.c` (220) en `test_upsert.c` (38)
naar Rust `#[test]`'s. Tests die falen worden `#[ignore]` met annotatie welke gap ze raken.

Verwacht resultaat: ~103/273 groen, ~170 `#[ignore]`.

### Fase 1.2 ✅ -- Gaps dichten

#### Batch A -- Blokkers (zonder deze start Plex niet)

| #  | Gap                                                | Tests | ~Regels |
|----|----------------------------------------------------|-------|---------|
| A1 | Upsert conflict targets (25+ table mapping)        | 28    | ~150    |
| A2 | Upsert special columns (COALESCE, GREATEST, RETURNING) | 11 | ~80     |
| A3 | `param_names` in FFI struct                         | 1     | ~40     |
| A4 | `datetime('now')` -> `NOW()`                        | 1     | ~20     |
| A5 | `instr` arg-order bugfix                            | 1     | ~5      |
| A6 | `AUTOINCREMENT` -> `SERIAL`                         | 2     | ~15     |
| A7 | `ALTER TABLE ADD COLUMN IF NOT EXISTS`               | 3     | ~30     |
| A8 | Mixed-case identifier quoting                       | 11    | ~100    |
| A9 | `json_each` -> `json_array_elements` + casts        | 2     | ~50     |
|    | **Subtotaal Batch A**                               | ~60   | ~490    |

#### Batch B -- Plex-specifieke query fixes

| #   | Gap                                                | Tests | ~Regels |
|-----|----------------------------------------------------|-------|---------|
| B1  | JSON operator `->>`                                | 5     | ~60     |
| B2  | Integer/text mismatch `::text` casts               | 9     | ~80     |
| B3  | Forward reference JOIN reordering                  | 5     | ~100    |
| B4  | Duplicate SET assignment dedup                     | 5     | ~120    |
| B5  | Collections query (metadata_type=18)               | 2     | ~40     |
| B6  | `simplify_typeof_fixup`                            | 1     | ~60     |
| B7  | typeof type-naam mapping                           | 2     | ~30     |
| B8  | HAVING cnt alias                                   | 1     | ~20     |
| B9  | `add_nulls_first_ordering` (SOCI compat)           | 5     | ~50     |
| B10 | strftime intervallen (3e argument)                 | 4     | ~50     |
| B11 | unixepoch intervallen                              | 1     | ~20     |
| B12 | Single-quote identifiers                           | 3     | ~30     |
| B13 | ON CONFLICT column unquoting                       | 1     | ~20     |
| B14 | COLLATE NOCASE -> ILIKE + ORDER BY LOWER           | 4     | ~40     |
| B15 | CASE booleans in AND/OR context                    | 9     | ~30     |
| B16 | sqlite_master completeness                         | 3     | ~40     |
| B17 | DISTINCT+ORDER BY (random(), GROUP BY variant)     | 2     | ~20     |
|     | **Subtotaal Batch B**                              | ~61   | ~810    |

#### Batch C -- FTS4

| #  | Gap                          | Tests | ~Regels |
|----|------------------------------|-------|---------|
| C1 | FTS4 MATCH -> tsquery        | 8     | ~250    |

#### Batch D -- Optimalisaties

| #  | Gap                           | Tests | ~Regels |
|----|-------------------------------|-------|---------|
| D1 | Thread-local translation cache | --    | ~120    |
| D2 | Operator spacing (verify AST) | 8     | ~0      |
| D3 | Overige NULL sorting varianten | 4    | ~20     |

### Fase 1.3 ✅ -- Vergelijkende validatie (offline replay)

1. Extraheer unieke SQLite queries uit `/tmp/plex_redirect_pg.log` (live Plex op main)
2. Bouw standalone CLI (`rust/sql-translator/examples/compare.rs`)
3. Vergelijk C-output vs Rust-output, fix tot 0 diffs
4. Plex blijft op main draaien -- develop wordt NIET live gebruikt

### Fase 1.4 ✅ -- Dual-mode flag

`PLEX_SQL_TRANSLATOR=compare`: C-output gebruiken, Rust-output loggen + diff.
Safety net voor later wanneer develop wel live getest wordt.

### Fase 1.5 ✅ -- C-translator verwijderen

1. Default omschakelen naar Rust
2. `sql_tr_*.c` + `sql_translator.c` + `sql_translator_internal.h` verwijderen
3. Makefile: `SQL_TR_OBJS` verwijderen
4. `sql_translator_rust_bridge.c` wordt enige C-wrapper

---

## LAAG 2: PG modules ✅ COMPLEET

### Migratievolgorde (bottom-up dependency graph)

```
pg_types.h (foundation)
    |
    +-- pg_config (Stap 2.1, 231r -> ~150r Rust)
    +-- pg_logging (Stap 2.2, 401r -> ~250r Rust)
    +-- pg_mem_telemetry (Stap 2.3, 101r -> ~80r Rust)
    +-- shim_alloc (Stap 2.4, 314r -> ~200r Rust)
    |
    +-- pg_query_cache (Stap 2.5, 409r -> ~300r Rust, afhankelijk van logging)
    |
    +-- pg_statement (Stap 2.6, 807r -> ~550r Rust, afhankelijk van cache+logging+config)
    |
    +-- pg_client (Stap 2.7, 1666r -> ~1100r Rust, afhankelijk van alles)
```

### Per module

#### Stap 2.1 ✅ -- pg_config (trivial)
- `struct PgConfig` met `once_cell::sync::Lazy`
- Pure functions: `should_redirect()`, `should_skip_sql()`, etc.
- Port `test_pg_config.c`

#### Stap 2.2 ✅ -- pg_logging (medium)
- `Mutex<BufWriter<File>>` met log rotation
- Fork-safety: Rust `pg_logging_reset()` vanuit C `pthread_atfork`
- Port `test_logging_deadlock.c`

#### Stap 2.3 ✅ -- pg_mem_telemetry (trivial)
- `AtomicU64` counters, opt-in via env var

#### Stap 2.4 ✅ -- shim_alloc (medium)
- Lock-free allocation tracker met `AtomicPtr` hash table
- Opt-in via `PLEX_PG_ALLOC_TRACK=1`

#### Stap 2.5 ✅ -- pg_query_cache (medium)
- `thread_local! { RefCell<QueryCache> }` -- geen mutexen
- `Arc<CachedResult>` voor ref-counting
- Port `test_query_cache.c`

#### Stap 2.6 ✅ -- pg_statement (hoog)
- `Arc<Mutex<PgStmt>>` vervangt handmatige ref_count
- `RwLock<HashMap<usize, Arc<PgStmt>>>` voor registry
- Port `test_statement_helpers.c`, `test_decltype_soci_compat.c`, etc.

#### Stap 2.7 ✅ -- pg_client (zeer hoog)
- Pool slots: `Vec<AtomicU32>` state + `Mutex<Option<PgConnection>>`
- TLS fast-path: `thread_local! { Cell<Option<(usize, u32)>> }`
- Fork-safety, health checks, reconnect, prepared stmt cache
- Port `test_pool_reaper.c`, `test_fork_safety.c`, `test_tls_cache.c`, etc.

### Laag 2 crate-structuur

```
rust/pg-core/
+-- Cargo.toml           (deps: libpq-sys, once_cell)
+-- src/
    +-- lib.rs
    +-- config.rs        (Stap 2.1)
    +-- logging.rs       (Stap 2.2)
    +-- telemetry.rs     (Stap 2.3)
    +-- alloc.rs         (Stap 2.4)
    +-- cache.rs         (Stap 2.5)
    +-- statement.rs     (Stap 2.6)
    +-- pool.rs          (Stap 2.7)
    +-- types.rs
    +-- ffi.rs
```

---

## Tijdlijn (afgerond)

| Fase    | Inhoud                              | Status       |
|---------|-------------------------------------|--------------|
| **1.1** | Port 273 C-tests naar Rust          | ✅ Compleet  |
| **1.2A**| Batch A: 9 blokker-gaps             | ✅ Compleet  |
| **1.2B**| Batch B: 17 Plex-specifieke fixes   | ✅ Compleet  |
| **1.2C**| Batch C: FTS4                       | ✅ Compleet  |
| **1.2D**| Batch D: Cache + randgevallen       | ✅ Compleet  |
| **1.3** | Offline vergelijkende validatie     | ✅ Compleet  |
| **1.4** | Dual-mode flag                      | ✅ Compleet  |
| **1.5** | C-translator verwijderen            | ✅ Compleet  |
| **2.1** | pg_config -> Rust                   | ✅ Compleet  |
| **2.2** | pg_logging -> Rust                  | ✅ Compleet  |
| **2.3** | pg_mem_telemetry -> Rust            | ✅ Compleet  |
| **2.4** | shim_alloc -> Rust                  | ✅ Compleet  |
| **2.5** | pg_query_cache -> Rust              | ✅ Compleet  |
| **2.6** | pg_statement -> Rust                | ✅ Compleet  |
| **2.7** | pg_client -> Rust                   | ✅ Compleet  |
|         | **Alles afgerond**                  | **22 feb 2026** |

---

## Regels (tijdens migratie)

- Plex draaide op main (`/Users/sander/plex-postgresql/`) tijdens de migratie
- Alle werk in `/Users/sander/plex-postgresql-develop/` (develop branch)
- Validatie via offline replay van queries uit het live logbestand
- Migratie afgerond en live gedeployed op 22 februari 2026
