# plex-postgresql

**Rith Plex Media Server le PostgreSQL in áit SQLite.**

[Read in English](README.md) | [Lees in het Nederlands](README.nl.md) | [Leer en Espa&ntilde;ol](README.es.md)

Leabharlann shim bheag a ghabhann glaonna SQLite Plex agus a sheolann chuig PostgreSQL iad. Ní gá cód foinse Plex a athrú.

| Ardán | Stádas |
|-------|--------|
| macOS | ✅ Tástáilte i dtáirgeadh |
| Linux (Docker) | ✅ Ag obair (tástáilte le tús agus rith) |
| Linux (Dúchasach) | ✅ Dénártha réamhthiomsaithe |

## Eisiúint is déanaí: v1.2.0

**Shim atá sábháilte i Rust** — 92% de na díthagairtí pointeoir amh curtha ar ceal. Tá loighic ghnó sábháilte i Rust anois; tá unsafe teoranta don teorainn FFI.

- 🆕 **92% laghdú unsafe** — díthagairtí pointeoir amh 806→59, feidhmeanna inmheánacha ag úsáid tagairtí sábháilte `&mut`
- 🆕 **Sceitheadh cuimhne deisithe (1.8GB→59MB)** — sceitheadh PGresult, ródú idirbheart, glantachán cached_result
- 🔧 **3 deadlock réitithe** — mutex ceangail athchúrsach, cosc ABBA, deisiú convoy
- 🔧 **11 rás sonraí deisithe** — cuntóirí adamhacha, seqlocks, OnceLock, réiteach crúcaí luath
- 🔧 **PgStmt le Vec** — leithdháileadh ar éileamh, 540 beart do cheisteanna gan paraiméadair (88KB roimhe seo)
- ✅ **Clippy glan** — nialas rabhadh le `-D warnings`

Íoslódáil: https://github.com/cgnl/plex-postgresql/releases/tag/v1.2.0

## Cén fáth PostgreSQL?

Oibríonn SQLite go maith i go leor cásanna, ach tá teorainn thábhachtach aige: **glasáil bunachar sonraí iomlán**.

- **Níos lú glasanna** — le PostgreSQL, is féidir scanadh agus athsheinm a rith le chéile go réidh.
- **Níos fearr le stóráil chianda** — úsáideach le rclone agus seirbhísí néil.
- **Níos fearr ar scála** — láimhseálann catalóga móra níos cobhsaí.
- **Uirlisí aitheanta** — `pg_dump` agus cliaint PostgreSQL le haghaidh cúltaca agus seiceáil.

## Tús Tapa (Docker)

An bealach is simplí chun Plex a rith le PostgreSQL:

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# Tosaigh Plex + PostgreSQL
docker compose up -d

# Féach ar logaí
docker compose logs -f plex
```

Tá Plex ar fáil ag http://localhost:8080

## Tús Tapa (macOS)

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.2.0/plex-postgresql-v1.2.0-macos.zip \
  -o /tmp/plex-pg-macos.zip
mkdir -p /tmp/plex-pg-macos && cd /tmp/plex-pg-macos
unzip /tmp/plex-pg-macos.zip
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
```

Tar éis nuashonrú Plex: rith `install_wrappers.sh` arís.

## Tús Tapa (Linux)

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

## Imirce ó SQLite

```bash
./scripts/migrate_sqlite_to_pg.sh   # SQLite → PostgreSQL
./scripts/migrate_pg_to_sqlite.sh   # PostgreSQL → SQLite (béite)
./scripts/doctor.sh                  # Seiceáil agus deisigh scéimre + sonraí
```

## Cumraíocht

| Athróg | Réamhshocrú | Cur Síos |
|--------|-------------|----------|
| `PLEX_PG_HOST` | localhost | Óstach PostgreSQL (nó eolaire soicéad cosúil le `/tmp`) |
| `PLEX_PG_PORT` | 5432 | Port PostgreSQL |
| `PLEX_PG_DATABASE` | plex | Ainm an bhunachair sonraí |
| `PLEX_PG_USER` | plex | Úsáideoir an bhunachair sonraí |
| `PLEX_PG_PASSWORD` | (folamh) | Pasfhocal an bhunachair sonraí |
| `PLEX_PG_SCHEMA` | plex | Ainm an scéimre |
| `PLEX_PG_POOL_SIZE` | 50 | Méid tosaigh an mhúnla ceangail |
| `PLEX_PG_LOG_LEVEL` | 1 | 0=EARRÁID, 1=EOLAS, 2=DÍFHABHTÚ |

## Conas a oibríonn sé

Gabhann an shim glaonna `sqlite3_*`, athscríobhann SQL SQLite go SQL PostgreSQL, agus ritheann é trí libpq.

```
Sraith 4+3: Interposer Rust        — fishhook, DYLD_INTERPOSE, LD_PRELOAD
Sraith 2:   Modúil PG Rust         — linn, ráiteas, taisce, cumraíocht, logáil
Sraith 1:   Aistritheoir SQL Rust   — aistriúchán iomlán AST SQLite → PostgreSQL
```

Tuilleadh sonraí teicniúla sa **[vicí](https://github.com/cgnl/plex-postgresql/wiki)**.

## Tástáil

```bash
cargo test --manifest-path rust/plex-pg-core/Cargo.toml   # Gach tástáil Rust
```

## Réiteach Fadhbanna

```bash
pg_isready -h localhost -U plex          # Seiceáil PostgreSQL
./scripts/doctor.sh                       # Seiceáil agus deisigh scéimre + sonraí
tail -50 /tmp/plex_redirect_pg.log       # Féach ar logaí (macOS)
docker compose logs -f plex              # Féach ar logaí (Docker)
```

Tuilleadh: **[vicí/Troubleshooting](https://github.com/cgnl/plex-postgresql/wiki/Troubleshooting)**

## Ceadúnas

MIT - Féach [LICENSE](LICENSE)

---
*Tionscadal neamhoifigiúil, gan baint le Plex Inc. Úsáid ar do phriacal féin.*
