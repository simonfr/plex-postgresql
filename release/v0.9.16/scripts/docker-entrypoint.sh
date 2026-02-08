#!/usr/bin/with-contenv bash
# Docker entrypoint for plex-postgresql
# Initializes PostgreSQL schema before Plex starts
# Uses with-contenv to access Docker environment variables in s6-overlay

set -e

# Migration library location (copied by Dockerfile)
MIGRATE_LIB="/usr/local/lib/plex-postgresql/migrate_lib.sh"

# Set up variables for migration library
# Auto-detect source SQLite database from common locations
detect_sqlite_db() {
    local locations=(
        # Explicit mount point
        "/source-db/com.plexapp.plugins.library.db"
        # Linux standard location (if host path mounted)
        "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
        # macOS location (if host path mounted)
        "/Users/*/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
        # Alternative Linux locations
        "/opt/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
        # Container's own database (last resort)
        "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
    )

    for pattern in "${locations[@]}"; do
        # Use glob expansion for wildcard patterns
        for db in $pattern; do
            if [[ -f "$db" ]]; then
                echo "$db"
                return 0
            fi
        done
    done

    # Default fallback
    echo "/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"
}

SQLITE_DB=$(detect_sqlite_db)
if [[ "$SQLITE_DB" != "/config/"* ]]; then
    echo "Found source SQLite database for migration: $SQLITE_DB"
fi
PG_HOST="${PLEX_PG_HOST:-postgres}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
SHIM_DIR="/usr/local/lib/plex-postgresql"

# Non-interactive mode for Docker (auto-migrate if PG is empty)
MIGRATION_INTERACTIVE="${MIGRATION_INTERACTIVE:-0}"

# Source migration library if available
if [[ -f "$MIGRATE_LIB" ]]; then
    source "$MIGRATE_LIB"
fi

# Wait for PostgreSQL to be ready
wait_for_postgres() {
    echo "Waiting for PostgreSQL at ${PLEX_PG_HOST}:${PLEX_PG_PORT}..."

    export PGHOST="${PLEX_PG_HOST:-postgres}"
    export PGPORT="${PLEX_PG_PORT:-5432}"
    export PGDATABASE="${PLEX_PG_DATABASE:-plex}"
    export PGUSER="${PLEX_PG_USER:-plex}"
    export PGPASSWORD="${PLEX_PG_PASSWORD:-plex}"

    local max_attempts=30
    local attempt=1

    while [ $attempt -le $max_attempts ]; do
        if psql -c "SELECT 1" >/dev/null 2>&1; then
            echo "PostgreSQL is ready!"
            return 0
        fi
        echo "Attempt $attempt/$max_attempts - PostgreSQL not ready, waiting..."
        sleep 2
        attempt=$((attempt + 1))
    done

    echo "ERROR: PostgreSQL did not become ready in time"
    return 1
}

# Initialize schema if needed
init_schema() {
    local schema="${PLEX_PG_SCHEMA:-plex}"
    local schema_file="/usr/local/lib/plex-postgresql/plex_schema.sql"

    psql -c "CREATE SCHEMA IF NOT EXISTS $schema;" 2>/dev/null || true

    local table_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')

    if [ "$table_count" -gt "0" ] 2>/dev/null; then
        echo "PostgreSQL schema '$schema' ready with $table_count tables"
        # Load sqlite_column_types metadata if table doesn't exist yet
        local types_file="/usr/local/lib/plex-postgresql/sqlite_column_types.sql"
        if [ -f "$types_file" ]; then
            local types_exists=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema' AND table_name = 'sqlite_column_types';" 2>/dev/null | tr -d ' ')
            if [ "$types_exists" = "0" ] 2>/dev/null; then
                echo "Loading sqlite_column_types metadata..."
                psql -f "$types_file" 2>/dev/null || true
            fi
        fi
    else
        echo "PostgreSQL schema '$schema' is empty, loading schema..."
        if [ -f "$schema_file" ]; then
            echo "Loading schema from $schema_file..."
            if psql -f "$schema_file" 2>&1; then
                local new_count=$(psql -t -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '$schema';" 2>/dev/null | tr -d ' ')
                echo "Schema loaded successfully! $new_count tables created."

                # NOTE: schema_migrations rows from the dump are kept intact.
                # The shim intercepts INSERT INTO schema_migrations and adds
                # ON CONFLICT DO NOTHING, so duplicate versions are silently ignored.
                # This prevents Plex from re-running all 446 migrations from scratch,
                # which causes DDL/schema divergence issues with the SQLite shadow DB.
                local migration_count=$(psql -t -c "SELECT COUNT(*) FROM ${schema}.schema_migrations;" 2>/dev/null | tr -d ' ')
                echo "schema_migrations has $migration_count entries (kept from dump, shim handles duplicates)"
            else
                echo "WARNING: Schema load had errors, continuing anyway..."
            fi
        else
            echo "WARNING: Schema file $schema_file not found!"
        fi
        # Load sqlite_column_types metadata after fresh schema load
        local types_file="/usr/local/lib/plex-postgresql/sqlite_column_types.sql"
        if [ -f "$types_file" ]; then
            echo "Loading sqlite_column_types metadata..."
            psql -f "$types_file" 2>/dev/null || true
        fi
    fi
}

# Sync schema_migrations from PostgreSQL to SQLite
# This ensures Plex doesn't try to re-run migrations that are already applied in PG
sync_schema_migrations_to_sqlite() {
    local db_file="$1"
    local db_name
    db_name=$(basename "$db_file")

    local pg_count=$(psql -t -c "SELECT COUNT(*) FROM ${PG_SCHEMA}.schema_migrations;" 2>/dev/null | tr -d ' ')
    local sqlite_count=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM schema_migrations;" 2>/dev/null || echo "0")

    if [ "$pg_count" -gt "$sqlite_count" ] 2>/dev/null; then
        echo "Syncing schema_migrations to SQLite ($sqlite_count → $pg_count rows)..."
        # Export from PG and import into SQLite
        psql -t -A -c "SELECT version FROM ${PG_SCHEMA}.schema_migrations ORDER BY version;" 2>/dev/null | while IFS= read -r version; do
            [ -z "$version" ] && continue
            sqlite3 "$db_file" "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('$version');" 2>/dev/null || true
        done
        local new_count=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM schema_migrations;" 2>/dev/null || echo "0")
        echo "SQLite schema_migrations now has $new_count entries"
    else
        echo "SQLite schema_migrations already in sync ($sqlite_count rows)"
    fi
}

# Pre-initialize a single SQLite database
init_single_sqlite_db() {
    local db_file="$1"
    local schema_file="$2"
    local db_name
    db_name=$(basename "$db_file")

    if [ ! -f "$db_file" ]; then
        echo "Pre-initializing SQLite database $db_name..."
        if [ -f "$schema_file" ]; then
            # Ignore errors from virtual tables (spellfix1, fts4, rtree)
            sqlite3 "$db_file" < "$schema_file" 2>&1 || true
            echo "SQLite database $db_name initialized"
        else
            echo "WARNING: Schema file not found: $schema_file"
        fi
        chown abc:abc "$db_file" 2>/dev/null || true
    else
        # Database exists, ensure it has the min_version column
        if ! sqlite3 "$db_file" "SELECT min_version FROM schema_migrations LIMIT 1" >/dev/null 2>&1; then
            echo "Adding min_version column to $db_name..."
            sqlite3 "$db_file" "ALTER TABLE schema_migrations ADD COLUMN min_version TEXT;" 2>/dev/null || true
        fi
    fi

    # Always sync migration versions from PG to SQLite
    # This prevents Plex from re-running migrations that are already in PostgreSQL
    sync_schema_migrations_to_sqlite "$db_file"
}

# Pre-initialize SQLite databases with correct schema
# This is needed because SOCI validates the SQLite schema before our shim can intercept
init_sqlite_schema() {
    local db_dir="/config/Library/Application Support/Plex Media Server/Plug-in Support/Databases"
    local schema_file="/usr/local/lib/plex-postgresql/sqlite_schema.sql"

    mkdir -p "$db_dir"

    # Initialize both databases explicitly
    init_single_sqlite_db "$db_dir/com.plexapp.plugins.library.db" "$schema_file"
    init_single_sqlite_db "$db_dir/com.plexapp.plugins.library.blobs.db" "$schema_file"
}

# Pre-create required Plex directories
# Prevents boost::filesystem errors when Plex scans for plugins and metadata
init_plex_directories() {
    local plex_dir="/config/Library/Application Support/Plex Media Server"
    
    echo "Ensuring required Plex directories exist..."
    
    # Create standard Plex directories
    mkdir -p "$plex_dir/Plug-ins"
    mkdir -p "$plex_dir/Metadata"
    mkdir -p "$plex_dir/Cache"
    mkdir -p "$plex_dir/Logs"
    
    # Set ownership to abc:abc (PUID:PGID from environment)
    chown -R abc:abc "$plex_dir" 2>/dev/null || true
    
    echo "Plex directories initialized"
}

# Locale setup removed — Plex's bundled musl+boost::locale handles locale internally.
# Setting LANG/LC_ALL/CHARSET can interfere with exception handling on aarch64.

# Verify PostgreSQL shim configuration
# Note: The shim is now injected at Docker build time via Dockerfile
# This function just verifies the configuration is in place
verify_plex_shim() {
    local shim_path="/usr/local/lib/plex-postgresql/db_interpose_pg.so"
    # Check both s6-overlay v3 (linuxserver) and v2 (plexinc) paths
    local s6_run=""
    if [ -f "/etc/s6-overlay/s6-rc.d/svc-plex/run" ]; then
        s6_run="/etc/s6-overlay/s6-rc.d/svc-plex/run"
    elif [ -f "/etc/services.d/plex/run" ]; then
        s6_run="/etc/services.d/plex/run"
    fi

    if [ -f "$shim_path" ]; then
        echo "PostgreSQL shim library found: $shim_path"
        if [ -n "$s6_run" ] && grep -q "LD_PRELOAD=" "$s6_run" 2>/dev/null; then
            echo "Plex run script configured for PostgreSQL shim (set at build time)"
        else
            echo "WARNING: Plex run script missing LD_PRELOAD - shim may not load!"
        fi
    else
        echo "Warning: PostgreSQL shim library not found at $shim_path"
    fi
}

# Main
echo "=== plex-postgresql entrypoint ==="
echo "PostgreSQL: ${PLEX_PG_USER}@${PLEX_PG_HOST}:${PLEX_PG_PORT}/${PLEX_PG_DATABASE}"

if [ -n "$PLEX_PG_HOST" ]; then
    wait_for_postgres
    init_schema

    # Run migration if source SQLite DB exists (mounted via -v)
    if [[ -f "$MIGRATE_LIB" ]] && [[ -f "$SQLITE_DB" ]]; then
        echo "Checking for data migration..."
        check_and_migrate || true
    fi

    init_plex_directories
    init_sqlite_schema
    verify_plex_shim
    
    # Final permission fix - ensure Plex can write to its directories
    # This must be done after all directories are created
    echo "Fixing final permissions..."
    chown -R abc:abc "/config/Library/Application Support/Plex Media Server" 2>/dev/null || true
else
    echo "PLEX_PG_HOST not set, skipping PostgreSQL initialization"
fi

echo "PostgreSQL initialization complete"
# When called as s6-overlay init script, just exit successfully
# s6 will continue with the rest of the init sequence
exit 0
