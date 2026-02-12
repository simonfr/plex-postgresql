# plex-postgresql Makefile
# Supports both macOS (DYLD_INTERPOSE) and Linux (LD_PRELOAD)

UNAME_S := $(shell uname -s)

PLEX_BIN ?= /Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server

# Compiler settings
ifeq ($(UNAME_S),Darwin)
    # macOS with Homebrew PostgreSQL
    CC = clang
    PG_INCLUDE = /opt/homebrew/opt/postgresql@15/include
    PG_LIB = /opt/homebrew/opt/postgresql@15/lib
    # Added -Isrc to find new headers
    # -fvisibility=hidden: hide internal symbols, only export sqlite3 interception functions
    CFLAGS = -Wall -Wextra -O2 -I$(PG_INCLUDE) -Iinclude -Isrc -fvisibility=hidden
    LDFLAGS = -L$(PG_LIB) -lpq
    TARGET = db_interpose_pg.dylib
    SOURCE = src/db_interpose_pg.c
    SHARED_FLAGS = -dynamiclib -undefined dynamic_lookup
else
    # Linux
    CC = gcc
    PG_INCLUDE = /usr/include/postgresql
    PG_LIB = /usr/lib
    CFLAGS = -Wall -Wextra -O2 -fPIC -I$(PG_INCLUDE) -Iinclude -Isrc
    LDFLAGS = -lpq -lsqlite3 -ldl -lpthread
    TARGET = db_interpose_pg.so
    SOURCE = src/db_interpose_pg_linux.c
    SHARED_FLAGS = -shared
endif

# SQL Translator modules
SQL_TR_OBJS = src/sql_translator.o src/sql_tr_helpers.o src/sql_tr_placeholders.o \
              src/sql_tr_functions.o src/sql_tr_query.o src/sql_tr_groupby.o src/sql_tr_types.o \
              src/sql_tr_quotes.o src/sql_tr_keywords.o src/sql_tr_upsert.o

# PG modules
PG_MODULES = src/pg_config.o src/pg_logging.o src/pg_client.o src/pg_statement.o src/pg_query_cache.o

# DB Interpose modules - shared between Mac and Linux
DB_INTERPOSE_SHARED = src/db_interpose_common.o src/platform_backtrace.o src/db_interpose_open.o \
                      src/db_interpose_exec.o src/db_interpose_prepare.o src/db_interpose_bind.o \
                      src/db_interpose_step.o src/db_interpose_column.o src/db_interpose_value.o \
                      src/db_interpose_metadata.o

# Platform-specific core module
ifeq ($(UNAME_S),Darwin)
    DB_INTERPOSE_CORE = src/db_interpose_core.o
else
    DB_INTERPOSE_CORE = src/db_interpose_core_linux.o
endif

DB_INTERPOSE_OBJS = $(DB_INTERPOSE_CORE) $(DB_INTERPOSE_SHARED)

# All objects (macOS includes fishhook, Linux doesn't)
ifeq ($(UNAME_S),Darwin)
    OBJECTS = $(SQL_TR_OBJS) $(PG_MODULES) $(DB_INTERPOSE_OBJS) src/fishhook.o
else
    OBJECTS = $(SQL_TR_OBJS) $(PG_MODULES) $(DB_INTERPOSE_OBJS)
endif
LINUX_OBJECTS = $(SQL_TR_OBJS) $(PG_MODULES) $(DB_INTERPOSE_SHARED) src/db_interpose_core_linux.o

.PHONY: all clean install test macos linux run stop unit-test ci-test test-recursion test-crash test-params test-logging test-soci test-fork test-fts test-buffer test-reaper test-groupby test-upsert test-parity test-uri test-stmt-free test-bind-mismatch

all: $(TARGET)

# Build the shim library (auto-detect platform) - uses modular approach
$(TARGET): $(OBJECTS)
	$(CC) $(SHARED_FLAGS) -o $@ $(OBJECTS) $(CFLAGS) $(LDFLAGS)

# Explicit macOS build - always clean first to avoid corrupt object files
macos: clean
	@for src in $(SQL_TR_OBJS:.o=.c); do \
		obj=$$(echo $$src | sed 's/\.c$$/.o/'); \
		$(CC) -c -fPIC -o $$obj $$src $(CFLAGS); \
	done
	@for src in $(PG_MODULES:.o=.c); do \
		obj=$$(echo $$src | sed 's/\.c$$/.o/'); \
		$(CC) -c -fPIC -o $$obj $$src $(CFLAGS); \
	done
	@for src in $(DB_INTERPOSE_SHARED:.o=.c) src/db_interpose_core.c; do \
		obj=$$(echo $$src | sed 's/\.c$$/.o/'); \
		$(CC) -c -fPIC -o $$obj $$src $(CFLAGS); \
	done
	$(CC) -c -O2 -Iinclude -Isrc -o src/fishhook.o src/fishhook.c
	clang -dynamiclib -undefined dynamic_lookup -o db_interpose_pg.dylib $(OBJECTS) \
		-I/opt/homebrew/opt/postgresql@15/include -Iinclude -Isrc \
		-L/opt/homebrew/opt/postgresql@15/lib -lpq

# Explicit Linux build (modular - same structure as Mac)
linux: $(LINUX_OBJECTS)
	gcc -shared -fPIC -o db_interpose_pg.so $(LINUX_OBJECTS) \
		-I/usr/include/postgresql -Iinclude -Isrc \
		-lpq -lsqlite3 -ldl -lpthread

# Object rules
# SQL Translator module compilation rules
src/sql_translator.o: src/sql_translator.c include/sql_translator.h src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_helpers.o: src/sql_tr_helpers.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_placeholders.o: src/sql_tr_placeholders.c include/sql_translator.h src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_functions.o: src/sql_tr_functions.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_query.o: src/sql_tr_query.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_groupby.o: src/sql_tr_groupby.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_types.o: src/sql_tr_types.c include/sql_translator.h src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_quotes.o: src/sql_tr_quotes.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_keywords.o: src/sql_tr_keywords.c include/sql_translator.h src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_upsert.o: src/sql_tr_upsert.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_config.o: src/pg_config.c src/pg_config.h src/pg_types.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_logging.o: src/pg_logging.c src/pg_logging.h src/pg_types.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_client.o: src/pg_client.c src/pg_client.h src/pg_types.h src/pg_logging.h src/pg_config.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_statement.o: src/pg_statement.c src/pg_statement.h src/pg_types.h src/pg_logging.h src/pg_client.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_query_cache.o: src/pg_query_cache.c src/pg_query_cache.h src/pg_types.h src/pg_logging.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/fishhook.o: src/fishhook.c include/fishhook.h
	$(CC) -c -O2 -Iinclude -o $@ $<

# DB Interpose module compilation rules
src/db_interpose_common.o: src/db_interpose_common.c src/db_interpose.h src/db_interpose_common.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/platform_backtrace.o: src/platform_backtrace.c src/db_interpose.h src/db_interpose_common.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_core.o: src/db_interpose_core.c src/db_interpose.h src/db_interpose_common.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_core_linux.o: src/db_interpose_core_linux.c src/db_interpose.h src/db_interpose_common.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_open.o: src/db_interpose_open.c src/db_interpose.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_exec.o: src/db_interpose_exec.c src/db_interpose.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_prepare.o: src/db_interpose_prepare.c src/db_interpose.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_bind.o: src/db_interpose_bind.c src/db_interpose.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_step.o: src/db_interpose_step.c src/db_interpose.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_column.o: src/db_interpose_column.c src/db_interpose.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_value.o: src/db_interpose_value.c src/db_interpose.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/db_interpose_metadata.o: src/db_interpose_metadata.c src/db_interpose.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

# Clean build artifacts
clean:
	rm -f db_interpose_pg.dylib db_interpose_pg.so $(OBJECTS) $(PG_MODULES) $(DB_INTERPOSE_OBJS)

# Install to system location
install: $(TARGET)
ifeq ($(UNAME_S),Darwin)
	@mkdir -p /usr/local/lib/plex-postgresql
	cp $(TARGET) /usr/local/lib/plex-postgresql/
	@echo "Installed to /usr/local/lib/plex-postgresql/"
else
	@mkdir -p /usr/local/lib/plex-postgresql
	cp $(TARGET) /usr/local/lib/plex-postgresql/
	@ldconfig /usr/local/lib/plex-postgresql 2>/dev/null || true
	@echo "Installed to /usr/local/lib/plex-postgresql/"
endif

# Test the shim
test: $(TARGET)
	@echo "Testing shim library load..."
ifeq ($(UNAME_S),Darwin)
	@DYLD_INSERT_LIBRARIES=./$(TARGET) \
		PLEX_PG_HOST=localhost \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		/bin/echo "Shim loaded successfully"
else
	@LD_PRELOAD=./$(TARGET) \
		PLEX_PG_HOST=localhost \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		/bin/echo "Shim loaded successfully"
endif

# Development: rebuild and test
dev: clean all test

# Run Plex (macOS only)
run: $(TARGET)
ifeq ($(UNAME_S),Darwin)
	@echo "Starting Plex Media Server with PostgreSQL shim..."
	@pkill -f "Plex Media Server" 2>/dev/null || true
	@sleep 2
	@DYLD_INSERT_LIBRARIES="$(CURDIR)/db_interpose_pg.dylib" \
	PLEX_NO_SHADOW_SCAN=1 \
	PLEX_PG_HOST=$${PLEX_PG_HOST:-localhost} \
	PLEX_PG_PORT=$${PLEX_PG_PORT:-5432} \
	PLEX_PG_DATABASE=$${PLEX_PG_DATABASE:-plex} \
	PLEX_PG_USER=$${PLEX_PG_USER:-plex} \
	PLEX_PG_PASSWORD=$${PLEX_PG_PASSWORD:-plex} \
	PLEX_PG_SCHEMA=$${PLEX_PG_SCHEMA:-plex} \
	PLEX_PG_LOG_LEVEL=$${PLEX_PG_LOG_LEVEL:-ERROR} \
	PLEX_PG_LOG_FILE=$${PLEX_PG_LOG_FILE:-/tmp/plex_redirect_pg.log} \
	"$(PLEX_BIN)" >> $${PLEX_PG_LOG_FILE:-/tmp/plex_redirect_pg.log} 2>&1 &
	@echo "Plex started. Log: /tmp/plex_redirect_pg.log"
else
	@echo "Run target only supported on macOS"
endif

stop:
	@pkill -9 -f "Plex Media Server" 2>/dev/null || true
	@pkill -9 -f "Plex Plug-in" 2>/dev/null || true
	@echo "Plex stopped"

# ============================================================================
# Unit Tests
# ============================================================================

TEST_DIR = tests/src
TEST_BIN_DIR = tests/bin

# Test for recursion prevention and stack protection
$(TEST_BIN_DIR)/test_recursion: $(TEST_DIR)/test_recursion.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -Wall -Wextra

test-recursion: $(TEST_BIN_DIR)/test_recursion
	@echo ""
	@./$(TEST_BIN_DIR)/test_recursion
	@echo ""

# Test for crash scenarios from production history
$(TEST_BIN_DIR)/test_crash_scenarios: $(TEST_DIR)/test_crash_scenarios.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -Wall -Wextra

test-crash: $(TEST_BIN_DIR)/test_crash_scenarios
	@echo ""
	@./$(TEST_BIN_DIR)/test_crash_scenarios
	@echo ""

# macOS stack protection integration test (requires shim to be built)
$(TEST_BIN_DIR)/test_stack_macos: $(TEST_DIR)/test_stack_macos.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -Wall -Wextra

test-stack-macos: $(TARGET) $(TEST_BIN_DIR)/test_stack_macos
	@echo ""
	@./$(TEST_BIN_DIR)/test_stack_macos ./$(TARGET)
	@echo ""

# SQL translator unit tests (links against translator objects + logging)
$(TEST_BIN_DIR)/test_sql_translator: $(TEST_DIR)/test_sql_translator.c $(SQL_TR_OBJS) src/pg_logging.o
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< $(SQL_TR_OBJS) src/pg_logging.o -Iinclude -Isrc -Wall -Wextra

test-sql: $(TEST_BIN_DIR)/test_sql_translator
	@echo ""
	@./$(TEST_BIN_DIR)/test_sql_translator
	@echo ""

# Type normalization unit tests (standalone - tests decltype normalization)
$(TEST_BIN_DIR)/test_type_normalization: $(TEST_DIR)/test_type_normalization.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -Wall -Wextra

test-types: $(TEST_BIN_DIR)/test_type_normalization
	@echo ""
	@./$(TEST_BIN_DIR)/test_type_normalization
	@echo ""

# SOCI type compatibility tests (std::bad_cast prevention)
$(TEST_BIN_DIR)/test_decltype_soci_compat: $(TEST_DIR)/test_decltype_soci_compat.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -Wall -Wextra

test-soci: $(TEST_BIN_DIR)/test_decltype_soci_compat
	@echo ""
	@./$(TEST_BIN_DIR)/test_decltype_soci_compat
	@echo ""

# Query cache unit tests (standalone - tests cache logic without libpq)
$(TEST_BIN_DIR)/test_query_cache: $(TEST_DIR)/test_query_cache.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -Wall -Wextra

test-cache: $(TEST_BIN_DIR)/test_query_cache
	@echo ""
	@./$(TEST_BIN_DIR)/test_query_cache
	@echo ""

# TLS cache unit tests (thread-local storage caching)
$(TEST_BIN_DIR)/test_tls_cache: $(TEST_DIR)/test_tls_cache.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -Wall -Wextra

test-tls: $(TEST_BIN_DIR)/test_tls_cache
	@echo ""
	@./$(TEST_BIN_DIR)/test_tls_cache
	@echo ""

# Fork safety unit tests (pthread_atfork handlers)
# NOTE: These tests run WITHOUT the shim loaded - they test fork logic in isolation
$(TEST_BIN_DIR)/test_fork_safety: $(TEST_DIR)/test_fork_safety.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -Wall -Wextra

test-fork: $(TEST_BIN_DIR)/test_fork_safety
	@echo ""
	@./$(TEST_BIN_DIR)/test_fork_safety
	@echo ""

# Pool reaper unit tests (connection pool idle cleanup)
$(TEST_BIN_DIR)/test_pool_reaper: $(TEST_DIR)/test_pool_reaper.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -Wall -Wextra

test-reaper: $(TEST_BIN_DIR)/test_pool_reaper
	@echo ""
	@./$(TEST_BIN_DIR)/test_pool_reaper
	@echo ""

# Micro-benchmarks (shim component performance)
$(TEST_BIN_DIR)/test_benchmark: $(TEST_DIR)/test_benchmark.c $(SQL_TR_OBJS) src/pg_logging.o
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -O3 -o $@ $< $(SQL_TR_OBJS) src/pg_logging.o -Iinclude -Isrc -Wall -Wextra

benchmark: $(TEST_BIN_DIR)/test_benchmark
	@./$(TEST_BIN_DIR)/test_benchmark

# SQLite API function tests (tests with shim loaded)
$(TEST_BIN_DIR)/test_sqlite_api: $(TEST_DIR)/test_sqlite_api.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lsqlite3 -Wall -Wextra

# sqlite3_expanded_sql and boolean value conversion tests
$(TEST_BIN_DIR)/test_expanded_sql: $(TEST_DIR)/test_expanded_sql.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lsqlite3 -Wall -Wextra

test-expanded: $(TARGET) $(TEST_BIN_DIR)/test_expanded_sql
	@echo ""
ifeq ($(UNAME_S),Darwin)
	@DYLD_INSERT_LIBRARIES=./$(TARGET) \
		PLEX_PG_HOST=/tmp \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		./$(TEST_BIN_DIR)/test_expanded_sql
else
	@LD_PRELOAD=./$(TARGET) \
		PLEX_PG_HOST=localhost \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		./$(TEST_BIN_DIR)/test_expanded_sql
endif
	@echo ""

test-api: $(TARGET) $(TEST_BIN_DIR)/test_sqlite_api
	@echo ""
ifeq ($(UNAME_S),Darwin)
	@DYLD_INSERT_LIBRARIES=./$(TARGET) \
		PLEX_PG_HOST=/tmp \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		./$(TEST_BIN_DIR)/test_sqlite_api
else
	@LD_PRELOAD=./$(TARGET) \
		PLEX_PG_HOST=localhost \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		./$(TEST_BIN_DIR)/test_sqlite_api
endif
	@echo ""

# Bind parameter index tests (named parameter mapping)
$(TEST_BIN_DIR)/test_bind_parameter_index: $(TEST_DIR)/test_bind_parameter_index.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lsqlite3 -Wall -Wextra

test-params: $(TARGET) $(TEST_BIN_DIR)/test_bind_parameter_index
	@echo ""
ifeq ($(UNAME_S),Darwin)
	@DYLD_INSERT_LIBRARIES=./$(TARGET) \
		PLEX_PG_HOST=/tmp \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		./$(TEST_BIN_DIR)/test_bind_parameter_index
else
	@LD_PRELOAD=./$(TARGET) \
		PLEX_PG_HOST=localhost \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		./$(TEST_BIN_DIR)/test_bind_parameter_index
endif
	@echo ""

# Logging deadlock prevention tests
$(TEST_BIN_DIR)/test_logging_deadlock: $(TEST_DIR)/test_logging_deadlock.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -Wall -Wextra

test-logging: $(TEST_BIN_DIR)/test_logging_deadlock
	@echo ""
ifeq ($(UNAME_S),Darwin)
	@perl -e 'alarm 10; exec @ARGV' ./$(TEST_BIN_DIR)/test_logging_deadlock || echo "DEADLOCK DETECTED"
else
	@timeout 10 ./$(TEST_BIN_DIR)/test_logging_deadlock || echo "DEADLOCK DETECTED"
endif
	@echo ""

# Exception handler unit tests (C++ exception interception logic)
$(TEST_BIN_DIR)/test_exception_handler: $(TEST_DIR)/test_exception_handler.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -ldl -Wall -Wextra

test-exception: $(TEST_BIN_DIR)/test_exception_handler
	@echo ""
	@./$(TEST_BIN_DIR)/test_exception_handler
	@echo ""

# FTS escaped quote handling tests
$(TEST_BIN_DIR)/test_fts_quotes: $(TEST_DIR)/test_fts_quotes.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -Wall -Wextra

test-fts: $(TEST_BIN_DIR)/test_fts_quotes
	@echo ""
	@./$(TEST_BIN_DIR)/test_fts_quotes
	@echo ""

# Buffer pool unit tests (column_text buffer expansion)
$(TEST_BIN_DIR)/test_buffer_pool: $(TEST_DIR)/test_buffer_pool.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -lpthread -Wall -Wextra

test-buffer: $(TEST_BIN_DIR)/test_buffer_pool
	@echo ""
	@./$(TEST_BIN_DIR)/test_buffer_pool
	@echo ""

# GROUP BY rewriter unit tests
$(TEST_BIN_DIR)/test_group_by_rewriter: tests/test_group_by_rewriter.c src/sql_tr_groupby.o src/sql_tr_helpers.o src/pg_logging.o
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< src/sql_tr_groupby.o src/sql_tr_helpers.o src/pg_logging.o -Iinclude -Isrc -Wall -Wextra

test-groupby: $(TEST_BIN_DIR)/test_group_by_rewriter
	@echo ""
	@./$(TEST_BIN_DIR)/test_group_by_rewriter
	@echo ""

# UPSERT (INSERT OR REPLACE) unit tests
$(TEST_BIN_DIR)/test_upsert: $(TEST_DIR)/test_upsert.c $(SQL_TR_OBJS) src/pg_logging.o
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< $(SQL_TR_OBJS) src/pg_logging.o -Iinclude -Isrc -Wall -Wextra

test-upsert: $(TEST_BIN_DIR)/test_upsert
	@echo ""
	@./$(TEST_BIN_DIR)/test_upsert
	@echo ""

# SQL classification unit tests (should_redirect, should_skip_sql, is_write/read_operation)
$(TEST_BIN_DIR)/test_pg_config: $(TEST_DIR)/test_pg_config.c src/pg_config.c src/sql_tr_helpers.o src/pg_logging.o
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $(TEST_DIR)/test_pg_config.c src/pg_config.c src/sql_tr_helpers.o src/pg_logging.o -Iinclude -Isrc -I$(PG_INCLUDE) -Wall -Wextra

test-config: $(TEST_BIN_DIR)/test_pg_config
	@echo ""
	@./$(TEST_BIN_DIR)/test_pg_config
	@echo ""

# Bind helper unit tests (contains_binary_bytes, bytes_to_pg_hex)
$(TEST_BIN_DIR)/test_bind_helpers: $(TEST_DIR)/test_bind_helpers.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -Wall -Wextra

test-bind: $(TEST_BIN_DIR)/test_bind_helpers
	@echo ""
	@./$(TEST_BIN_DIR)/test_bind_helpers
	@echo ""

# Common helper unit tests (is_library_db_path, simple_str_replace)
$(TEST_BIN_DIR)/test_common_helpers: $(TEST_DIR)/test_common_helpers.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -Wall -Wextra

test-common: $(TEST_BIN_DIR)/test_common_helpers
	@echo ""
	@./$(TEST_BIN_DIR)/test_common_helpers
	@echo ""

# Platform parity unit tests (shared symbol loading, backtrace module)
$(TEST_BIN_DIR)/test_platform_parity: $(TEST_DIR)/test_platform_parity.c src/db_interpose_common.o src/platform_backtrace.o src/pg_logging.o
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< src/db_interpose_common.o src/platform_backtrace.o src/pg_logging.o -Iinclude -Isrc -Wall -Wextra -lsqlite3 -ldl

test-parity: $(TEST_BIN_DIR)/test_platform_parity
	@echo ""
	@./$(TEST_BIN_DIR)/test_platform_parity
	@echo ""

# Statement helper unit tests (metadata_settings upsert, metadata ID extraction)
$(TEST_BIN_DIR)/test_statement_helpers: $(TEST_DIR)/test_statement_helpers.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -Wall -Wextra

test-statement: $(TEST_BIN_DIR)/test_statement_helpers
	@echo ""
	@./$(TEST_BIN_DIR)/test_statement_helpers
	@echo ""

# Statement free sweep regression test (ensures all param slots are freed)
$(TEST_BIN_DIR)/test_stmt_free_param_sweep: $(TEST_DIR)/test_stmt_free_param_sweep.c src/pg_statement.o src/sql_tr_helpers.o
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< src/pg_statement.o src/sql_tr_helpers.o -Iinclude -Isrc -I$(PG_INCLUDE) -Wall -Wextra $(LDFLAGS) -lpthread

test-stmt-free: $(TEST_BIN_DIR)/test_stmt_free_param_sweep
	@echo ""
	@MallocStackLogging=1 leaks -q --atExit -- ./$(TEST_BIN_DIR)/test_stmt_free_param_sweep
	@echo ""

# Bind index mismatch regression (idx > param_count cleanup safety)
$(TEST_BIN_DIR)/test_bind_index_mismatch_cleanup: $(TEST_DIR)/test_bind_index_mismatch_cleanup.c src/pg_statement.o src/sql_tr_helpers.o
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< src/pg_statement.o src/sql_tr_helpers.o -Iinclude -Isrc -I$(PG_INCLUDE) -Wall -Wextra $(LDFLAGS) -lpthread

test-bind-mismatch: $(TEST_BIN_DIR)/test_bind_index_mismatch_cleanup
	@echo ""
	@MallocStackLogging=1 leaks -q --atExit -- ./$(TEST_BIN_DIR)/test_bind_index_mismatch_cleanup
	@echo ""

# URI rewrite tests (server:// -> library://)
$(TEST_BIN_DIR)/test_uri_rewrite: $(TEST_DIR)/test_uri_rewrite.c
	@mkdir -p $(TEST_BIN_DIR)
	$(CC) -o $@ $< -Wall -Wextra

test-uri: $(TEST_BIN_DIR)/test_uri_rewrite
	@echo ""
	@./$(TEST_BIN_DIR)/test_uri_rewrite
	@echo ""

# Run all unit tests
unit-test: test-recursion test-crash test-sql test-groupby test-upsert test-types test-soci test-cache test-tls test-fork test-reaper test-buffer test-api test-expanded test-params test-logging test-exception test-fts test-config test-bind test-common test-statement test-stmt-free test-bind-mismatch test-parity test-uri
	@echo "All unit tests complete."

# CI-safe subset: excludes tests needing LD_PRELOAD + shim (test-api, test-expanded, test-params)
ci-test: test-recursion test-crash test-sql test-groupby test-upsert test-types test-soci test-cache test-tls test-fork test-reaper test-buffer test-logging test-exception test-fts test-config test-bind test-common test-statement test-stmt-free test-bind-mismatch test-parity test-uri
	@echo "All CI unit tests complete."

# ============================================================================
# Release builds
# ============================================================================

RELEASE_DIR = release
VERSION = $(shell cat VERSION 2>/dev/null || echo "dev")

# Architecture-specific builds (macOS only)
# Note: db_interpose_common.c is already included in DB_INTERPOSE_SHARED
release-arm64: clean
	@echo "Building arm64..."
	@for src in $(SQL_TR_OBJS:.o=.c) $(PG_MODULES:.o=.c) $(DB_INTERPOSE_SHARED:.o=.c) src/db_interpose_core.c src/fishhook.c; do \
		obj=$$(echo $$src | sed 's/\.c$$/.o/'); \
		$(CC) -c -fPIC -arch arm64 -o $$obj $$src $(CFLAGS); \
	done
	$(CC) -dynamiclib -undefined dynamic_lookup -arch arm64 -o db_interpose_pg-arm64.dylib $(OBJECTS) $(CFLAGS) $(LDFLAGS)
	@echo "Built db_interpose_pg-arm64.dylib"

release-x86_64: clean
	@echo "Building x86_64..."
	@for src in $(SQL_TR_OBJS:.o=.c) $(PG_MODULES:.o=.c) $(DB_INTERPOSE_SHARED:.o=.c) src/db_interpose_core.c src/fishhook.c; do \
		obj=$$(echo $$src | sed 's/\.c$$/.o/'); \
		$(CC) -c -fPIC -arch x86_64 -o $$obj $$src $(CFLAGS); \
	done
	$(CC) -dynamiclib -undefined dynamic_lookup -arch x86_64 -o db_interpose_pg-x86_64.dylib $(OBJECTS) $(CFLAGS) $(LDFLAGS)
	@echo "Built db_interpose_pg-x86_64.dylib"

# Universal binary (both architectures)
release-universal:
	@echo "Building universal binary for v$(VERSION)..."
	@mkdir -p $(RELEASE_DIR)/v$(VERSION)
	@# Build arm64
	@$(MAKE) clean >/dev/null
	@for src in $(SQL_TR_OBJS:.o=.c) $(PG_MODULES:.o=.c) $(DB_INTERPOSE_SHARED:.o=.c) src/db_interpose_core.c src/fishhook.c; do \
		obj=$$(echo $$src | sed 's/\.c$$/.o/'); \
		$(CC) -c -fPIC -arch arm64 -o $$obj $$src $(CFLAGS) 2>/dev/null; \
	done
	@$(CC) -dynamiclib -undefined dynamic_lookup -arch arm64 -o $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-arm64.dylib $(OBJECTS) $(CFLAGS) $(LDFLAGS)
	@echo "  ✓ arm64"
	@# Build x86_64
	@$(MAKE) clean >/dev/null
	@for src in $(SQL_TR_OBJS:.o=.c) $(PG_MODULES:.o=.c) $(DB_INTERPOSE_SHARED:.o=.c) src/db_interpose_core.c src/fishhook.c; do \
		obj=$$(echo $$src | sed 's/\.c$$/.o/'); \
		$(CC) -c -fPIC -arch x86_64 -o $$obj $$src $(CFLAGS) 2>/dev/null; \
	done
	@$(CC) -dynamiclib -undefined dynamic_lookup -arch x86_64 -o $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-x86_64.dylib $(OBJECTS) $(CFLAGS) $(LDFLAGS)
	@echo "  ✓ x86_64"
	@# Create universal binary
	@lipo -create \
		$(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-arm64.dylib \
		$(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-x86_64.dylib \
		-output $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg.dylib
	@echo "  ✓ universal"
	@# Show result
	@echo ""
	@echo "Release v$(VERSION) built:"
	@ls -lh $(RELEASE_DIR)/v$(VERSION)/*.dylib
	@echo ""
	@file $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg.dylib

# Create macOS release tarball
release: release-universal
	@echo "Packaging macOS release..."
	@cd $(RELEASE_DIR)/v$(VERSION) && \
		mkdir -p scripts && \
		cp ../../README.md ../../LICENSE ../../CHANGELOG.md ../../RELEASE_NOTES.md ../../INSTALL.md . 2>/dev/null || true && \
		cp ../../scripts/install_wrappers.sh scripts/ && \
		cp ../../scripts/install_wrappers_linux.sh scripts/ && \
		cp ../../scripts/uninstall_wrappers.sh scripts/ && \
		cp ../../scripts/uninstall_wrappers_linux.sh scripts/ && \
		cp ../../scripts/migrate_sqlite_to_pg.sh scripts/ && \
		cp ../../scripts/docker-entrypoint.sh scripts/ && \
		tar -czf ../plex-postgresql-v$(VERSION)-macos.tar.gz \
			*.dylib README.md LICENSE CHANGELOG.md RELEASE_NOTES.md INSTALL.md scripts/
	@echo "  ✓ $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-macos.tar.gz"
	@ls -lh $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-macos.tar.gz

# Build Linux release (requires Docker)
release-linux:
	@echo "Building Linux binaries via Docker..."
	@mkdir -p $(RELEASE_DIR)/v$(VERSION)
	@# Build aarch64
	docker buildx build --platform linux/arm64 --target builder -t plex-pg-builder-arm64 --load . 2>&1 | tail -3
	docker rm -f plex-pg-extract 2>/dev/null || true
	docker create --name plex-pg-extract plex-pg-builder-arm64
	docker cp plex-pg-extract:/libs/db_interpose_pg.so $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-linux-aarch64.so
	docker cp plex-pg-extract:/libs/libpq.so.5 $(RELEASE_DIR)/v$(VERSION)/
	docker rm plex-pg-extract
	@echo "  ✓ linux-aarch64"
	@# Build x86_64
	docker buildx build --platform linux/amd64 --target builder -t plex-pg-builder-amd64 --load . 2>&1 | tail -3
	docker rm -f plex-pg-extract 2>/dev/null || true
	docker create --name plex-pg-extract plex-pg-builder-amd64
	docker cp plex-pg-extract:/libs/db_interpose_pg.so $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-linux-x86_64.so
	docker rm plex-pg-extract
	@echo "  ✓ linux-x86_64"
	@# Package
	@cd $(RELEASE_DIR)/v$(VERSION) && \
		mkdir -p scripts && \
		cp ../../README.md ../../LICENSE ../../CHANGELOG.md ../../RELEASE_NOTES.md ../../INSTALL.md . 2>/dev/null || true && \
		cp ../../scripts/install_wrappers.sh scripts/ && \
		cp ../../scripts/install_wrappers_linux.sh scripts/ && \
		cp ../../scripts/uninstall_wrappers.sh scripts/ && \
		cp ../../scripts/uninstall_wrappers_linux.sh scripts/ && \
		cp ../../scripts/migrate_sqlite_to_pg.sh scripts/ && \
		cp ../../scripts/docker-entrypoint.sh scripts/ && \
		tar -czf ../plex-postgresql-v$(VERSION)-linux.tar.gz \
			db_interpose_pg-linux-*.so libpq.so.5 \
			README.md LICENSE CHANGELOG.md RELEASE_NOTES.md INSTALL.md scripts/
	@echo "  ✓ $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-linux.tar.gz"
	@ls -lh $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-linux.tar.gz

# Full release (macOS + Linux)
release-all: release release-linux
	@echo ""
	@echo "=== Release v$(VERSION) Complete ==="
	@ls -lh $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-*.tar.gz
