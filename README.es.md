# plex-postgresql

[![en](https://img.shields.io/badge/lang-en-red.svg)](README.md)
[![es](https://img.shields.io/badge/lang-es-yellow.svg)](README.es.md)

**Ejecuta Plex Media Server con PostgreSQL en lugar de SQLite.**

Una librería shim pequeña que captura las llamadas SQLite de Plex y las envía a PostgreSQL. No necesitas modificar Plex.

| Plataforma | Estado |
|------------|--------|
| macOS | ✅ Probado en producción |
| Linux (Docker) | ✅ Funciona (init y ejecución probados, no probado en producción) |
| Linux (Nativo) | ⚠️ No probado |

## Última versión: v0.9.36

**Fix arranque Docker standalone** — elimina `chown -R` lento que causaba retrasos de minutos en librerías grandes.

- ✅ **Fix chown Docker (PR #7):** eliminado `chown -R plex:plex` del entrypoint standalone — Plex gestiona los permisos
- ✅ Recuperación tras reinicio de PG (Issue #8): reintentos pool + step (v0.9.34)
- ✅ **278 tests unitarios** (220 SQL + 41 shadow elimination + 17 connection isolation)

Descarga: https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.36

## ¿Por qué PostgreSQL?

SQLite funciona bien en muchos casos, pero tiene una limitación importante: **bloqueo de base de datos**.

- **Menos bloqueos** - con PostgreSQL, escaneos y reproducción conviven mejor.
- **Mejor para almacenamiento remoto** - útil con rclone y servicios en la nube.
- **Mejor en bibliotecas grandes** - maneja catálogos grandes con más estabilidad.
- **Herramientas conocidas** - `pg_dump` y clientes PostgreSQL para backup y revisión.

## Inicio Rápido (Docker)

La forma más simple de ejecutar Plex con PostgreSQL:

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# Iniciar Plex + PostgreSQL
docker-compose up -d

# Ver logs
docker-compose logs -f plex
```

Plex estará disponible en http://localhost:8080

PostgreSQL se configura automáticamente y crea el esquema inicial.

### Configuración

Edita `docker-compose.yml` para personalizar:

```yaml
environment:
  - PLEX_PG_HOST=postgres
  - PLEX_PG_PORT=5432
  - PLEX_PG_DATABASE=plex
  - PLEX_PG_USER=plex
  - PLEX_PG_PASSWORD=plex
  - PLEX_PG_SCHEMA=plex
  - PLEX_PG_POOL_SIZE=50
```

Monta tus medios:
```yaml
volumes:
  - /ruta/a/medios:/media:ro
```

## Inicio Rápido (macOS)

El instalador copia la librería shim dentro de `Plex Media Server.app`, parchea los binarios y configura el wrapper. Todo queda dentro del app bundle de Plex.

### 1. Configurar PostgreSQL

```bash
brew install postgresql@15
brew services start postgresql@15

createuser plex
createdb -O plex plex
psql -d plex -c "ALTER USER plex PASSWORD 'plex';"
```

### 2. Instalar (ZIP recomendado)

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.36/plex-postgresql-v0.9.36-macos.zip -o /tmp/plex-pg-macos.zip
mkdir -p /tmp/plex-pg-macos && cd /tmp/plex-pg-macos
unzip /tmp/plex-pg-macos.zip

pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
```

### Opción desde código fuente

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make clean && make

pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
```

### 3. Iniciar Plex

```bash
open "/Applications/Plex Media Server.app"
```

El shim se inyecta automáticamente. Ver logs: `tail -f /tmp/plex_redirect_pg.log`

Después de una actualización de Plex, ejecuta `install_wrappers.sh` de nuevo.

### Desinstalar

```bash
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/uninstall_wrappers.sh
```

## Inicio Rápido (Linux Nativo)

### 1. Configurar PostgreSQL

```bash
sudo apt install postgresql-15
sudo -u postgres createuser plex
sudo -u postgres createdb -O plex plex
sudo -u postgres psql -c "ALTER USER plex PASSWORD 'plex';"
psql -U plex -d plex -c "CREATE SCHEMA plex;"
```

### 2. Instalar (ZIP recomendado)

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.36/plex-postgresql-v0.9.36-linux.zip -o /tmp/plex-pg-linux.zip
mkdir -p /tmp/plex-pg && cd /tmp/plex-pg
unzip /tmp/plex-pg-linux.zip

# Instalar shim y wrappers
sudo mkdir -p /usr/local/lib/plex-postgresql
if [ "$(uname -m)" = "x86_64" ]; then
  sudo install -m 755 db_interpose_pg-linux-x86_64.so /usr/local/lib/plex-postgresql/db_interpose_pg.so
else
  sudo install -m 755 db_interpose_pg-linux-aarch64.so /usr/local/lib/plex-postgresql/db_interpose_pg.so
fi
sudo ./scripts/install_wrappers_linux.sh
```

### Opción desde código fuente

```bash
sudo apt install build-essential libsqlite3-dev libpq-dev

git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make linux
sudo make install

sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh
```

### 3. Configurar e Iniciar

```bash
# Añadir a /etc/default/plexmediaserver:
# PLEX_PG_HOST=localhost
# PLEX_PG_DATABASE=plex
# PLEX_PG_USER=plex
# PLEX_PG_PASSWORD=plex

sudo systemctl start plexmediaserver
```

### Desinstalar

```bash
sudo systemctl stop plexmediaserver
sudo ./scripts/uninstall_wrappers_linux.sh
```

## Configuración

| Variable | Predeterminado | Descripción |
|----------|----------------|-------------|
| `PLEX_PG_HOST` | localhost | Host de PostgreSQL |
| `PLEX_PG_PORT` | 5432 | Puerto de PostgreSQL |
| `PLEX_PG_DATABASE` | plex | Nombre de la base de datos |
| `PLEX_PG_USER` | plex | Usuario de la base de datos |
| `PLEX_PG_PASSWORD` | (vacío) | Contraseña de la base de datos |
| `PLEX_PG_SCHEMA` | plex | Nombre del esquema |
| `PLEX_PG_POOL_SIZE` | 50 | Tamaño inicial del pool de conexiones (crece automáticamente hasta 200) |
| `PLEX_PG_IDLE_TIMEOUT` | 300 | Segundos antes de cerrar conexiones inactivas |
| `PLEX_PG_LOG_LEVEL` | 1 | 0=ERROR, 1=INFO, 2=DEBUG |

## Cómo Funciona

```
macOS:  Plex → SQLite API → DYLD_INTERPOSE shim → Traductor SQL → PostgreSQL
Linux:  Plex → SQLite API → LD_PRELOAD shim    → Traductor SQL → PostgreSQL
Docker: Plex → SQLite API → LD_PRELOAD shim    → Traductor SQL → PostgreSQL (contenedor)
```

El shim captura llamadas `sqlite3_*`, traduce SQL de SQLite a PostgreSQL y lo ejecuta con libpq.

### Características Principales

- **Pool de conexiones** - Reutiliza conexiones PostgreSQL
- **Traducción SQL** - Convierte sintaxis SQLite → PostgreSQL
- **Prepared statements** - Usa caché de consultas para mejor rendimiento
- **Inicialización del esquema** - Crea el esquema PostgreSQL en el primer arranque

## Solución de Problemas

```bash
# Verificar PostgreSQL
pg_isready -h localhost -U plex

# Ver logs (macOS)
tail -50 /tmp/plex_redirect_pg.log

# Ver logs (Docker)
docker-compose logs -f plex

# Analizar fallbacks
./scripts/analyze_fallbacks.sh
```

### Problemas Comunes

**Plex no inicia**: verifica que PostgreSQL esté activo y accesible.

**Errores de base de datos**: Asegúrate de que el esquema existe: `psql -U plex -d plex -c "CREATE SCHEMA IF NOT EXISTS plex;"`

**Conflicto de puerto Docker**: Cambia el puerto en `docker-compose.yml` si 8080 está en uso.

## Licencia

MIT - Ver [LICENSE](LICENSE)

---
*Proyecto no oficial, no afiliado con Plex Inc. Usar bajo tu propio riesgo.*
