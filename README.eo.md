# plex-postgresql

**Rulu Plex Media Server kun PostgreSQL anstataŭ SQLite.**

[Read in English](README.md) | [Lees in het Nederlands](README.nl.md) | [Léigh as Gaeilge](README.ga.md) | [Leer en Espa&ntilde;ol](README.es.md)

Malgranda shim-biblioteko kiu kaptas la SQLite-vokojn de Plex kaj sendas ilin al PostgreSQL. Vi ne bezonas ŝanĝi la fontkodon de Plex.

| Platformo | Stato |
|-----------|-------|
| macOS | ✅ Testita en produktado |
| Linux (Docker) | ✅ Funkcias (inicialigo kaj rulado testitaj) |
| Linux (Indiĝena) | ✅ Antaŭkompilitaj binaroj |

## Plej nova versio: v1.2.0

**Rust-sekura shim** — 92% de la nesekuraj krudaj montrilaj dereferencoj forigitaj. La komerca logiko estas nun Rust-sekura; unsafe limigita al la FFI-limo.

- 🆕 **92% redukto de unsafe** — krudaj montrilaj dereferencoj 806→59, internaj funkcioj uzas sekurajn `&mut` referencojn
- 🆕 **Memorliko riparita (1.8GB→59MB)** — PGresult-liko, transakcio-direktado, cached_result-purigado
- 🔧 **3 ŝlosblokadoj forigitaj** — rekursa konekt-mutex, ABBA-prevento, konvojo-riparo
- 🔧 **11 datenaj raskondiĉoj riparitaj** — atomaj nombriloj, seqlocks, OnceLock, frua hoko-rezolucio
- 🔧 **PgStmt kun Vec** — laŭbezone asignado, 540 bajtoj por demandoj sen parametroj (antaŭe 88KB)
- ✅ **Clippy pura** — nul avertoj kun `-D warnings`

Elŝutu: https://github.com/cgnl/plex-postgresql/releases/tag/v1.2.0

## Kial PostgreSQL?

SQLite bone funkcias en multaj kazoj, sed ĝi havas gravan limigon: **tutdatumbaza ŝlosado**.

- **Malpli da ŝlosadoj** — kun PostgreSQL, skanadoj kaj ludado povas kuri kune pli glate.
- **Pli bona por fora stokado** — utila kun rclone kaj nubaj servoj.
- **Pli bona ĉe granda skalo** — traktas grandajn katalogojn pli stabile.
- **Konataj iloj** — `pg_dump` kaj PostgreSQL-klientoj por sekurkopiadoj kaj kontroloj.

## Rapida Komenco (Docker)

La plej simpla maniero ruli Plex kun PostgreSQL:

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# Komencu Plex + PostgreSQL
docker compose up -d

# Rigardu protokolojn
docker compose logs -f plex
```

Plex disponeblas ĉe http://localhost:8080

## Rapida Komenco (macOS)

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.2.0/plex-postgresql-v1.2.0-macos.zip \
  -o /tmp/plex-pg-macos.zip
mkdir -p /tmp/plex-pg-macos && cd /tmp/plex-pg-macos
unzip /tmp/plex-pg-macos.zip
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
```

Post Plex-ĝisdatigo: rerulu `install_wrappers.sh`.

## Rapida Komenco (Linux)

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.2.0/plex-postgresql-v1.2.0-linux.zip \
  -o /tmp/plex-pg-linux.zip
mkdir -p /tmp/plex-pg-linux && cd /tmp/plex-pg-linux
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

## Migrado de SQLite

```bash
./scripts/migrate_sqlite_to_pg.sh   # SQLite → PostgreSQL
./scripts/migrate_pg_to_sqlite.sh   # PostgreSQL → SQLite (beta)
./scripts/doctor.sh                  # Kontrolu kaj riparu skemon + datumojn
```

## Agordo

| Variablo | Defaŭlto | Priskribo |
|----------|----------|-----------|
| `PLEX_PG_HOST` | localhost | PostgreSQL-gastiga (aŭ socket-dosierujo kiel `/tmp`) |
| `PLEX_PG_PORT` | 5432 | PostgreSQL-pordo |
| `PLEX_PG_DATABASE` | plex | Datumbaznomo |
| `PLEX_PG_USER` | plex | Datumbaz-uzanto |
| `PLEX_PG_PASSWORD` | (malplena) | Datumbaz-pasvorto |
| `PLEX_PG_SCHEMA` | plex | Skemnomo |
| `PLEX_PG_POOL_SIZE` | 50 | Komenca konektaro-grandeco |
| `PLEX_PG_LOG_LEVEL` | 1 | 0=ERARO, 1=INFO, 2=SENCIMIGO |

## Kiel ĝi funkcias

La shim kaptas `sqlite3_*`-vokojn, reverklas SQLite-SQL al PostgreSQL-SQL, kaj plenumas ĝin per libpq.

```
Tavolo 4+3: Rust-interposer         — fishhook, DYLD_INTERPOSE, LD_PRELOAD
Tavolo 2:   Rust PG-moduloj         — konektaro, deklaro, kaŝmemoro, agordo, protokolado
Tavolo 1:   Rust SQL-tradukilo       — plena AST-bazita SQLite → PostgreSQL traduko
```

Pliaj teknikaj detaloj en la **[vikio](https://github.com/cgnl/plex-postgresql/wiki)**.

## Testado

```bash
cargo test --manifest-path rust/plex-pg-core/Cargo.toml   # Ĉiuj Rust-testoj
```

## Problemsolvado

```bash
pg_isready -h localhost -U plex          # Kontrolu PostgreSQL
./scripts/doctor.sh                       # Kontrolu kaj riparu skemon + datumojn
tail -50 /tmp/plex_redirect_pg.log       # Rigardu protokolojn (macOS)
docker compose logs -f plex              # Rigardu protokolojn (Docker)
```

Pliaj: **[vikio/Troubleshooting](https://github.com/cgnl/plex-postgresql/wiki/Troubleshooting)**

## Licenco

MIT - Vidu [LICENSE](LICENSE)

---
*Neoficiala projekto, ne ligita al Plex Inc. Uzu je via propra risko.*
