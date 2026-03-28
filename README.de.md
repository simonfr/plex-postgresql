# plex-postgresql

**Plex Media Server mit PostgreSQL statt SQLite betreiben.**

[Read in English](README.md) | [Lees in het Nederlands](README.nl.md) | [Léigh as Gaeilge](README.ga.md) | [Legu en Esperanto](README.eo.md) | [Leer en Espa&ntilde;ol](README.es.md)

Eine kleine Shim-Bibliothek, die die SQLite-Aufrufe von Plex abfaengt und an PostgreSQL weiterleitet. Der Plex-Quellcode muss nicht veraendert werden.

| Plattform | Status |
|-----------|--------|
| macOS | ✅ Im Produktivbetrieb getestet |
| Linux (Docker) | ✅ Funktioniert (Initialisierung und Ausfuehrung getestet) |
| Linux (Nativ) | ✅ Vorkompilierte Binaerdateien |

## Aktuelle Version: v1.2.0

**Rust-sicherer Shim** — 92% der unsicheren Rohzeiger-Dereferenzierungen eliminiert. Die Geschaeftslogik ist jetzt Rust-sicher; unsafe auf die FFI-Grenze beschraenkt.

- 🆕 **92% Unsafe-Reduktion** — Rohzeiger-Dereferenzierungen 806→59, interne Funktionen verwenden sichere `&mut`-Referenzen
- 🆕 **Speicherleck behoben (1.8GB→59MB)** — PGresult-Leck, Transaktionsrouting, cached_result-Bereinigung
- 🔧 **3 Deadlocks beseitigt** — rekursiver Verbindungs-Mutex, ABBA-Praevention, Konvoi-Behebung
- 🔧 **11 Data Races behoben** — atomare Zaehler, Seqlocks, OnceLock, fruehe Hook-Aufloesung
- 🔧 **PgStmt mit Vec** — bedarfsgesteuerte Zuweisung, 540 Bytes fuer Abfragen ohne Parameter (vorher 88KB)
- ✅ **Clippy sauber** — null Warnungen mit `-D warnings`

Download: https://github.com/cgnl/plex-postgresql/releases/tag/v1.2.0

## Warum PostgreSQL?

SQLite funktioniert in vielen Faellen gut, hat aber eine wichtige Einschraenkung: **datenbankweite Sperrung**.

- **Weniger Sperren** — mit PostgreSQL koennen Scans und Wiedergabe besser gleichzeitig laufen.
- **Besser fuer Remote-Speicher** — nuetzlich mit rclone und Cloud-Speicherdiensten.
- **Besser bei grossen Bibliotheken** — verarbeitet grosse Kataloge stabiler.
- **Bekannte Werkzeuge** — `pg_dump` und PostgreSQL-Clients fuer Backups und Pruefungen.

## Schnellstart (Docker)

Der einfachste Weg, Plex mit PostgreSQL zu betreiben:

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# Plex + PostgreSQL starten
docker compose up -d

# Protokolle anzeigen
docker compose logs -f plex
```

Plex ist unter http://localhost:8080 erreichbar.

## Schnellstart (macOS)

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.2.0/plex-postgresql-v1.2.0-macos.zip \
  -o /tmp/plex-pg-macos.zip
mkdir -p /tmp/plex-pg-macos && cd /tmp/plex-pg-macos
unzip /tmp/plex-pg-macos.zip
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
```

Nach einem Plex-Update: `install_wrappers.sh` erneut ausfuehren.

## Schnellstart (Linux)

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

## Migration von SQLite

```bash
./scripts/migrate_sqlite_to_pg.sh   # SQLite → PostgreSQL
./scripts/migrate_pg_to_sqlite.sh   # PostgreSQL → SQLite (Beta)
./scripts/doctor.sh                  # Schema + Daten pruefen und reparieren
```

## Konfiguration

| Variable | Standard | Beschreibung |
|----------|----------|-------------|
| `PLEX_PG_HOST` | localhost | PostgreSQL-Host (oder Socket-Verzeichnis wie `/tmp`) |
| `PLEX_PG_PORT` | 5432 | PostgreSQL-Port |
| `PLEX_PG_DATABASE` | plex | Datenbankname |
| `PLEX_PG_USER` | plex | Datenbankbenutzer |
| `PLEX_PG_PASSWORD` | (leer) | Datenbank-Passwort |
| `PLEX_PG_SCHEMA` | plex | Schema-Name |
| `PLEX_PG_POOL_SIZE` | 50 | Anfangsgroesse des Verbindungspools |
| `PLEX_PG_LOG_LEVEL` | 1 | 0=FEHLER, 1=INFO, 2=DEBUG |

## Wie es funktioniert

Der Shim faengt `sqlite3_*`-Aufrufe ab, schreibt SQLite-SQL in PostgreSQL-SQL um und fuehrt es ueber libpq aus.

```
Schicht 4+3: Rust-Interposer        — fishhook, DYLD_INTERPOSE, LD_PRELOAD
Schicht 2:   Rust PG-Module         — Pool, Statement, Cache, Konfiguration, Protokollierung
Schicht 1:   Rust SQL-Uebersetzer    — vollstaendig AST-basierte SQLite → PostgreSQL Uebersetzung
```

Weitere technische Details im **[Wiki](https://github.com/cgnl/plex-postgresql/wiki)**.

## Tests

```bash
cargo test --manifest-path rust/plex-pg-core/Cargo.toml   # Alle Rust-Tests
```

## Fehlerbehebung

```bash
pg_isready -h localhost -U plex          # PostgreSQL pruefen
./scripts/doctor.sh                       # Schema + Daten pruefen und reparieren
tail -50 /tmp/plex_redirect_pg.log       # Protokolle anzeigen (macOS)
docker compose logs -f plex              # Protokolle anzeigen (Docker)
```

Mehr: **[Wiki/Troubleshooting](https://github.com/cgnl/plex-postgresql/wiki/Troubleshooting)**

## Lizenz

MIT - Siehe [LICENSE](LICENSE)

---
*Inoffizielles Projekt, nicht mit Plex Inc. verbunden. Nutzung auf eigene Gefahr.*
