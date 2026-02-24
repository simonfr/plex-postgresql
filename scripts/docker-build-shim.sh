#!/bin/sh
# Shared Alpine/musl builder for plex-postgresql Docker images.
# Builds PostgreSQL libpq (without OpenSSL), Rust core, shim .so, and /libs payload.

set -eu

WITH_NOOP=0
if [ "${1:-}" = "--with-noop" ]; then
    WITH_NOOP=1
fi

echo "=== Building PostgreSQL/libpq (musl) ==="
cd /build
curl -L https://ftp.postgresql.org/pub/source/v15.10/postgresql-15.10.tar.gz | tar xz

cd /build/postgresql-15.10
ARCH="$(uname -m)"
if [ "$ARCH" = "x86_64" ]; then
    PG_CFLAGS='-O0 -mno-sse4.2'
else
    PG_CFLAGS='-O2'
fi

CFLAGS="$PG_CFLAGS" ac_cv_func_getaddrinfo=yes ./configure --prefix=/usr/local/pgsql \
    --without-readline \
    --without-zlib \
    --without-openssl \
    --without-icu

cd src/include && make install
cd ../interfaces/libpq && make && make install
cd ../../bin/pg_config && make && make install

echo "=== Building Rust core ==="
cd /build/rust/plex-pg-core
cargo build --release

echo "=== Building shim .so ==="
cd /build
ARCH="$(uname -m)"
echo "Building for architecture: $ARCH"
if [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then
    ARCH_FLAGS="-mno-outline-atomics"
else
    ARCH_FLAGS=""
fi

gcc -shared -fPIC -O2 -fno-stack-protector \
    -std=c11 -D_GNU_SOURCE $ARCH_FLAGS \
    -o db_interpose_pg.so \
    src/runtime/db_interpose_core_linux.c \
    src/runtime/db_interpose_common.c src/runtime/platform_backtrace.c \
    src/interpose/db_interpose_open.c src/interpose/db_interpose_exec.c \
    src/interpose/db_interpose_prepare.c src/interpose/db_interpose_bind.c \
    src/interpose/db_interpose_step.c src/interpose/db_interpose_txn_utils.c src/interpose/db_interpose_step_write_utils.c src/interpose/db_interpose_step_cached_read_utils.c src/interpose/db_interpose_column.c \
    src/interpose/db_interpose_value.c src/interpose/db_interpose_metadata.c \
    src/support/exception_what.cpp \
    src/rust_bridge/sql_translator_rust_bridge.c src/support/str_utils.c \
    src/pg/pg_config.c src/pg/pg_logging.c \
    src/pg/pg_client.c src/pg/pg_statement.c src/pg/pg_query_cache.c \
    src/pg/pg_mem_telemetry.c src/support/shim_alloc.c \
    rust/plex-pg-core/target/release/libplex_pg_core.a \
    -Iinclude -Isrc -I/usr/local/pgsql/include -I/usr/include \
    -L/usr/local/pgsql/lib -lpq \
    -lstdc++ \
    -ldl -lpthread \
    -Wl,-rpath,/usr/local/lib/plex-postgresql \
    -Wl,-rpath,/usr/lib/plexmediaserver/lib

echo "=== Shim dependencies ==="
LD_LIBRARY_PATH=/usr/local/pgsql/lib ldd db_interpose_pg.so || true

echo "=== Collecting runtime libs ==="
mkdir -p /libs
cp db_interpose_pg.so /libs/
cp /usr/local/pgsql/lib/libpq.so.5* /libs/
cp /usr/lib/libgcc_s.so.1 /libs/
cp /usr/lib/libstdc++.so.6* /libs/

if [ "$WITH_NOOP" = "1" ]; then
    echo "=== Building static noop binary for CrashUploader replacement ==="
    printf '#include <unistd.h>\nint main(void){_exit(0);}\n' > noop.c
    gcc -static -o noop noop.c
    cp noop /libs/
fi

ls -la /libs/
