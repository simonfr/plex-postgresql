#!/bin/bash
set -e
cd /tmp/shim_build

echo "=== Building Rust plex-pg-core ==="
cd rust/plex-pg-core && cargo build --release && cd /tmp/shim_build

echo "=== Compiling shim objects (musl-compatible) ==="
echo "  (no C objects; Rust-only build)"

echo "=== Linking shim (against musl libc + Rust staticlib) ==="
gcc -shared -fPIC -fno-stack-protector -mno-outline-atomics -nodefaultlibs \
    -o db_interpose_pg.so \
    -Wl,--whole-archive rust/plex-pg-core/target/release/libplex_pg_core.a -Wl,--no-whole-archive \
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
