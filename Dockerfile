# Dockerfile for plex-postgresql
# Build with Alpine 3.15 which has musl 1.2.2 - same as Plex's bundled musl!

FROM alpine:3.15 AS builder

# Install build dependencies
RUN apk add --no-cache \
    build-base \
    sqlite-dev \
    linux-headers \
    curl \
    perl

# Verify musl version matches Plex (1.2.2)
RUN /lib/ld-musl-*.so.1 --version 2>&1 | head -2

WORKDIR /build

# Download and build PostgreSQL with minimal features (just libpq)
RUN curl -L https://ftp.postgresql.org/pub/source/v15.10/postgresql-15.10.tar.gz | tar xz
RUN cd postgresql-15.10 && \
    # Configure WITHOUT OpenSSL to avoid ENGINE symbol conflicts
    ./configure --prefix=/usr/local/pgsql \
        --without-readline \
        --without-zlib \
        --without-openssl \
        --without-icu && \
    # Build and install include files first
    cd src/include && make install && \
    # Build and install libpq
    cd ../interfaces/libpq && make && make install && \
    # Build pg_config for headers
    cd ../../bin/pg_config && make && make install

# Copy source files
COPY src/ src/
COPY include/ include/

# Build shim with musl 1.2.2 (same as Plex)
# Compiler flags match build_shim_musl.sh for consistency and performance
# Note: Can't use -nodefaultlibs here because Plex's musl isn't available during build
# Architecture-specific flags: -mno-outline-atomics is ARM64-only
RUN ARCH=$(uname -m) && \
    echo "Building for architecture: $ARCH" && \
    if [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then \
        ARCH_FLAGS="-mno-outline-atomics"; \
        echo "ARM64 detected: adding -mno-outline-atomics flag"; \
    else \
        ARCH_FLAGS=""; \
        echo "x86_64 detected: skipping ARM-specific flags"; \
    fi && \
    gcc -shared -fPIC -O2 -fno-stack-protector \
        -std=c11 -D_XOPEN_SOURCE=700 $ARCH_FLAGS \
        -o db_interpose_pg.so \
        src/db_interpose_core_linux.c \
        src/db_interpose_common.c \
        src/db_interpose_open.c src/db_interpose_exec.c \
        src/db_interpose_prepare.c src/db_interpose_bind.c \
        src/db_interpose_step.c src/db_interpose_column.c \
        src/db_interpose_metadata.c \
        src/sql_translator.c src/sql_tr_helpers.c src/sql_tr_placeholders.c \
        src/sql_tr_functions.c src/sql_tr_query.c src/sql_tr_groupby.c \
        src/sql_tr_types.c src/sql_tr_quotes.c src/sql_tr_keywords.c \
        src/sql_tr_upsert.c src/pg_config.c src/pg_logging.c \
        src/pg_client.c src/pg_statement.c src/pg_query_cache.c \
        -I/usr/local/pgsql/include -I/usr/include -Iinclude -Isrc \
        -L/usr/local/pgsql/lib -lpq \
        -ldl -lpthread \
        -Wl,-rpath,/usr/local/lib/plex-postgresql \
        -Wl,-rpath,/usr/lib/plexmediaserver/lib

# Check dependencies
RUN echo "=== Shim dependencies ===" && (LD_LIBRARY_PATH=/usr/local/pgsql/lib ldd db_interpose_pg.so || true)

# Gather libraries
RUN mkdir -p /libs && \
    cp db_interpose_pg.so /libs/ && \
    cp /usr/local/pgsql/lib/libpq.so.5* /libs/ && \
    ls -la /libs/

# Runtime stage
FROM linuxserver/plex:latest

# Install PostgreSQL client for health checks, sqlite3 for schema fixes, gdb for debugging
# Also install locales - required for boost::locale in Plex
RUN apt-get update && apt-get install -y --no-install-recommends \
    postgresql-client \
    sqlite3 \
    gdb \
    locales \
    && rm -rf /var/lib/apt/lists/* \
    && echo "en_US.UTF-8 UTF-8" >> /etc/locale.gen \
    && locale-gen en_US.UTF-8

# Set locale environment variables for boost::locale compatibility
# CHARSET is required for boost::locale to work correctly (prevents invalid_charset_error)
ENV LANG=en_US.UTF-8
ENV LC_ALL=en_US.UTF-8
ENV LANGUAGE=en_US.UTF-8
ENV CHARSET=UTF-8

RUN mkdir -p /usr/local/lib/plex-postgresql

# Create symlinks for musl compatibility (architecture-specific)
# Our shim was built with Alpine which expects libc.musl-{arch}.so.1
# but Plex bundles musl as libc.so
RUN ARCH=$(uname -m) && \
    echo "Creating musl symlink for architecture: $ARCH" && \
    if [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then \
        MUSL_ARCH="aarch64"; \
    elif [ "$ARCH" = "x86_64" ]; then \
        MUSL_ARCH="x86_64"; \
    else \
        echo "Warning: Unknown architecture $ARCH, using $ARCH as-is"; \
        MUSL_ARCH="$ARCH"; \
    fi && \
    ln -sf /usr/lib/plexmediaserver/lib/libc.so /usr/local/lib/plex-postgresql/libc.musl-${MUSL_ARCH}.so.1 && \
    echo "Created symlink: libc.musl-${MUSL_ARCH}.so.1 -> /usr/lib/plexmediaserver/lib/libc.so"

COPY --from=builder /libs/*.so* /usr/local/lib/plex-postgresql/

COPY schema/plex_schema.sql /usr/local/lib/plex-postgresql/
COPY schema/sqlite_schema.sql /usr/local/lib/plex-postgresql/
COPY scripts/migrate_lib.sh /usr/local/lib/plex-postgresql/

# Copy the initialization script for s6-overlay
# This will run BEFORE Plex starts as part of the init sequence
COPY scripts/docker-entrypoint.sh /usr/local/lib/plex-postgresql/docker-entrypoint.sh
RUN chmod +x /usr/local/lib/plex-postgresql/docker-entrypoint.sh

# Create s6-overlay init script to run our initialization
RUN mkdir -p /etc/s6-overlay/s6-rc.d/init-plex-postgresql && \
    echo "oneshot" > /etc/s6-overlay/s6-rc.d/init-plex-postgresql/type && \
    echo "/usr/local/lib/plex-postgresql/docker-entrypoint.sh" > /etc/s6-overlay/s6-rc.d/init-plex-postgresql/up && \
    chmod +x /etc/s6-overlay/s6-rc.d/init-plex-postgresql/up && \
    mkdir -p /etc/s6-overlay/s6-rc.d/user/contents.d && \
    touch /etc/s6-overlay/s6-rc.d/user/contents.d/init-plex-postgresql
