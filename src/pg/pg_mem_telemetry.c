/*
 * PostgreSQL Shim - Memory Telemetry (thin FFI wrapper)
 *
 * All logic lives in rust/plex-pg-core/src/pg_mem_telemetry.rs.
 * This file is a thin C wrapper that forwards every call to the Rust
 * implementation, following the same hybrid pattern used for pg_config
 * and pg_logging.
 */

#include "pg_mem_telemetry.h"
#include "db_interpose_rust.h"

int pg_mem_telemetry_enabled(void) {
    return rust_mem_telemetry_enabled();
}

void pg_mem_telemetry_add(pg_mem_counter_t counter, size_t bytes, unsigned long long events) {
    rust_mem_telemetry_add((int)counter, (unsigned long long)bytes, events);
}

void pg_mem_telemetry_maybe_log(void) {
    rust_mem_telemetry_maybe_log();
}
