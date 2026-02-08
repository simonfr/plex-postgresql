# plex-postgresql

[![en](https://img.shields.io/badge/lang-en-red.svg)](README.md)
[![es](https://img.shields.io/badge/lang-es-yellow.svg)](README.es.md)

**Ejecuta Plex Media Server con PostgreSQL en lugar de SQLite.**

Una biblioteca shim que intercepta las llamadas SQLite de Plex y las redirige a PostgreSQL. Sin modificaciones a Plex.

| Plataforma | Estado |
|------------|--------|
| macOS | ✅ Probado en producción |
| Linux (Docker) | ✅ Funciona (init y ejecución probados, no probado en producción) |
| Linux (Nativo) | ⚠️ No probado |

## Última versión

- **v0.9.16**
- Descarga: https://github.com/cgnl/plex-postgresql/releases/tag/v0.9.16
- Formato de assets: solo ZIP
  - `plex-postgresql-v0.9.16-macos.zip`
  - `plex-postgresql-v0.9.16-linux.zip`

## ¿Por qué PostgreSQL?

SQLite es excelente para la mayoría de instalaciones de Plex, pero tiene una limitación importante: **bloqueo de base de datos**.

- **Sin bloqueos** - SQLite bloquea toda la base de datos durante escrituras. Los escaneos bloquean la reproducción. Escaneos concurrentes se ponen en cola. Con PostgreSQL, todo se ejecuta simultáneamente - escanea tus bibliotecas mientras transmites sin interrupciones.
- **Almacenamiento remoto** - Mejores patrones de I/O para rclone, Real-Debrid o configuraciones en la nube.
- **Bibliotecas grandes** - El optimizador de PostgreSQL maneja eficientemente más de 10K películas y 50K episodios.
- **Herramientas estándar** - pg_dump para backups, replicación, cualquier cliente PostgreSQL para depuración.

## Inicio Rápido (Docker)

La forma más fácil de ejecutar Plex con PostgreSQL:

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# Iniciar Plex + PostgreSQL
docker-compose up -d

# Ver logs
docker-compose logs -f plex
```

Plex estará disponible en http://localhost:8080

PostgreSQL se configura automáticamente con inicialización del esquema.

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

### 1. Configurar PostgreSQL

```bash
brew install postgresql@17
brew services start postgresql@17

createuser plex
createdb -O plex plex
psql -d plex -c "ALTER USER plex PASSWORD 'plex';"
psql -U plex -d plex -c "CREATE SCHEMA plex;"
```

### 2. Compilar e Instalar

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make clean && make

# Detener Plex, instalar wrappers
pkill -x "Plex Media Server" 2>/dev/null
./scripts/install_wrappers.sh
```

### Opción precompilada (ZIP)

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.16/plex-postgresql-v0.9.16-macos.zip -o /tmp/plex-pg-macos.zip
mkdir -p /tmp/plex-pg && cd /tmp/plex-pg
unzip /tmp/plex-pg-macos.zip

# Ejecutar instalador de wrappers desde el ZIP extraído
./scripts/install_wrappers.sh
```

### 3. Iniciar Plex

```bash
open "/Applications/Plex Media Server.app"
```

El shim se inyecta automáticamente. Ver logs: `tail -f /tmp/plex_redirect_pg.log`

### Desinstalar

```bash
pkill -x "Plex Media Server" 2>/dev/null
./scripts/uninstall_wrappers.sh
```

## Inicio Rápido (Linux Nativo) - No Probado

### 1. Configurar PostgreSQL

```bash
sudo apt install postgresql-15
sudo -u postgres createuser plex
sudo -u postgres createdb -O plex plex
sudo -u postgres psql -c "ALTER USER plex PASSWORD 'plex';"
psql -U plex -d plex -c "CREATE SCHEMA plex;"
```

### 2. Compilar e Instalar

```bash
# Instalar dependencias
sudo apt install build-essential libsqlite3-dev libpq-dev

git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make linux
sudo make install

# Detener Plex, instalar wrappers
sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh
```

### Opción precompilada (ZIP)

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v0.9.16/plex-postgresql-v0.9.16-linux.zip -o /tmp/plex-pg-linux.zip
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
| `PLEX_PG_POOL_SIZE` | 50 | Tamaño del pool de conexiones (máx 100) |
| `PLEX_PG_LOG_LEVEL` | 1 | 0=ERROR, 1=INFO, 2=DEBUG |

## Cómo Funciona

```
macOS:  Plex → SQLite API → DYLD_INTERPOSE shim → Traductor SQL → PostgreSQL
Linux:  Plex → SQLite API → LD_PRELOAD shim    → Traductor SQL → PostgreSQL
Docker: Plex → SQLite API → LD_PRELOAD shim    → Traductor SQL → PostgreSQL (contenedor)
```

El shim intercepta todas las llamadas `sqlite3_*`, traduce la sintaxis SQL (placeholders, funciones, tipos) y ejecuta en PostgreSQL via libpq.

### Características Principales

- **Pool de conexiones** - Reutilización eficiente de conexiones PostgreSQL
- **Traducción SQL** - Conversión automática de sintaxis SQLite → PostgreSQL
- **Prepared statements** - Caché de consultas para rendimiento
- **Inicialización del esquema** - Crea automáticamente el esquema PostgreSQL en el primer inicio

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

**Plex no inicia**: Verifica que PostgreSQL esté ejecutándose y accesible.

**Errores de base de datos**: Asegúrate de que el esquema existe: `psql -U plex -d plex -c "CREATE SCHEMA IF NOT EXISTS plex;"`

**Conflicto de puerto Docker**: Cambia el puerto en `docker-compose.yml` si 8080 está en uso.

## Licencia

MIT - Ver [LICENSE](LICENSE)

---
*Proyecto no oficial, no afiliado con Plex Inc. Usar bajo tu propio riesgo.*
