#!/bin/bash
set -e
cd /tmp/shim_build

echo "=== Building Rust plex-pg-core ==="
cd rust/plex-pg-core && cargo build --release && cd /tmp/shim_build

echo "=== Compiling shim objects (musl-compatible) ==="

# List of Linux source files (C interposer + Rust bridge + PG module shims)
LINUX_FILES="
runtime/db_interpose_core_linux.c
runtime/db_interpose_common.c
runtime/platform_backtrace.c
interpose/db_interpose_open.c
interpose/db_interpose_exec.c
interpose/db_interpose_prepare.c
interpose/db_interpose_bind.c
interpose/db_interpose_step.c
interpose/db_interpose_column.c
interpose/db_interpose_value.c
interpose/db_interpose_metadata.c
support/exception_what.cpp
rust_bridge/sql_translator_rust_bridge.c
support/str_utils.c
pg/pg_config.c
pg/pg_logging.c
pg/pg_client.c
pg/pg_statement.c
pg/pg_query_cache.c
pg/pg_mem_telemetry.c
support/shim_alloc.c
"

# Compile each source file with musl-compatible flags
for f in $LINUX_FILES; do
    obj="${f%.*}.o"
    echo "  Compiling $f -> $obj"
    mkdir -p "$(dirname "src/$obj")"
    if [[ "$f" == *.cpp ]]; then
        g++ -c -fPIC -O2 -fno-stack-protector \
            -std=c++17 -D_GNU_SOURCE -mno-outline-atomics \
            -Iinclude -Isrc -I/usr/include/postgresql \
            -o "src/$obj" "src/$f" 2>&1 || { echo "FAILED: $f"; exit 1; }
    else
        gcc -c -fPIC -O2 -fno-stack-protector \
            -std=c11 -D_GNU_SOURCE -mno-outline-atomics \
            -Iinclude -Isrc -I/usr/include/postgresql \
            -o "src/$obj" "src/$f" 2>&1 || { echo "FAILED: $f"; exit 1; }
    fi
done

echo "=== Linking shim (against musl libc + Rust staticlib) ==="
gcc -shared -fPIC -fno-stack-protector -mno-outline-atomics -nodefaultlibs \
    -o db_interpose_pg.so \
    src/runtime/db_interpose_core_linux.o \
    src/runtime/db_interpose_common.o src/runtime/platform_backtrace.o \
    src/interpose/db_interpose_open.o src/interpose/db_interpose_exec.o \
    src/interpose/db_interpose_prepare.o src/interpose/db_interpose_bind.o \
    src/interpose/db_interpose_step.o src/interpose/db_interpose_column.o \
    src/interpose/db_interpose_value.o src/interpose/db_interpose_metadata.o \
    src/support/exception_what.o \
    src/rust_bridge/sql_translator_rust_bridge.o src/support/str_utils.o \
    src/pg/pg_config.o src/pg/pg_logging.o \
    src/pg/pg_client.o src/pg/pg_statement.o src/pg/pg_query_cache.o \
    src/pg/pg_mem_telemetry.o src/support/shim_alloc.o \
    rust/plex-pg-core/target/release/libplex_pg_core.a \
    -lstdc++ \
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
