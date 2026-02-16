#!/bin/bash
set -e
cd /tmp/shim_build

echo "=== Compiling shim objects (musl-compatible) ==="

# List of Linux source files
LINUX_FILES="
db_interpose_core_linux.c
db_interpose_common.c
platform_backtrace.c
db_interpose_open.c
db_interpose_exec.c
db_interpose_prepare.c
db_interpose_bind.c
db_interpose_step.c
db_interpose_column.c
db_interpose_value.c
db_interpose_metadata.c
sql_translator.c
sql_tr_helpers.c
sql_tr_placeholders.c
sql_tr_functions.c
sql_tr_query.c
sql_tr_groupby.c
sql_tr_types.c
sql_tr_quotes.c
sql_tr_keywords.c
sql_tr_upsert.c
pg_config.c
pg_logging.c
pg_client.c
pg_statement.c
pg_query_cache.c
pg_mem_telemetry.c
shim_alloc.c
"

# Compile each source file with musl-compatible flags
for f in $LINUX_FILES; do
    obj=$(basename "$f" .c).o
    echo "  Compiling $f -> $obj"
    gcc -c -fPIC -O2 -fno-stack-protector \
        -std=c11 -D_XOPEN_SOURCE=700 -mno-outline-atomics \
        -I/usr/include/postgresql -Iinclude -Isrc \
        -o "src/$obj" "src/$f" 2>&1 || { echo "FAILED: $f"; exit 1; }
done

echo "=== Linking shim (against musl libc) ==="
gcc -shared -fPIC -fno-stack-protector -mno-outline-atomics -nodefaultlibs \
    -o db_interpose_pg.so \
    src/db_interpose_core_linux.o \
    src/db_interpose_common.o src/platform_backtrace.o \
    src/db_interpose_open.o src/db_interpose_exec.o \
    src/db_interpose_prepare.o src/db_interpose_bind.o \
    src/db_interpose_step.o src/db_interpose_column.o \
    src/db_interpose_value.o src/db_interpose_metadata.o \
    src/sql_translator.o src/sql_tr_helpers.o src/sql_tr_placeholders.o \
    src/sql_tr_functions.o src/sql_tr_query.o src/sql_tr_groupby.o \
    src/sql_tr_types.o src/sql_tr_quotes.o src/sql_tr_keywords.o \
    src/sql_tr_upsert.o src/pg_config.o src/pg_logging.o \
    src/pg_client.o src/pg_statement.o src/pg_query_cache.o \
    src/pg_mem_telemetry.o src/shim_alloc.o \
    -Wl,-rpath,/usr/local/lib/plex-postgresql \
    -Wl,-rpath,/usr/lib/plexmediaserver/lib \
    -L/usr/local/lib/plex-postgresql -l:libpq.so.5 \
    -L/usr/lib/plexmediaserver/lib -l:libc.so

echo "=== Installing shim ==="
cp db_interpose_pg.so /usr/local/lib/plex-postgresql/
ls -la /usr/local/lib/plex-postgresql/db_interpose_pg.so

echo "=== Checking dependencies ==="
LD_LIBRARY_PATH=/usr/lib/plexmediaserver/lib:/usr/local/lib/plex-postgresql ldd /usr/local/lib/plex-postgresql/db_interpose_pg.so 2>&1 || true

echo "=== Build complete ==="
