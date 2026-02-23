/*
 * Central Rust FFI declarations used by C modules.
 */

#ifndef DB_INTERPOSE_RUST_H
#define DB_INTERPOSE_RUST_H

#include <stddef.h>
#include <stdint.h>
#include <sqlite3.h>
#include <libpq-fe.h>

#include "pg_types.h"

typedef struct {
    char *normalized_sql;
    char **param_values;
    int param_count;
} normalized_sql_t;

/* db_interpose helpers */
uint32_t rust_decltype_hash(const char *str);
const char* rust_pg_udt_to_sqlite_decltype(const char *udt_name);
const char* rust_normalize_sqlite_decltype(const char *plex_type);
int rust_validate_utf8(const char *ptr, size_t len);
int rust_rewrite_server_library_uri(const char *input, char *out, size_t out_len);
int rust_trace_list_contains_idx(const char *list, int idx);
int rust_trace_list_any_token_in_haystack(const char *list, const char *haystack);
int rust_format_epoch_to_datetime_utc(long long epoch, char *out, size_t out_len);
int rust_load_badcast_config(int *enabled_out,
                             char *idx_out, size_t idx_len,
                             char *thread_out, size_t thread_len,
                             char *sql_out, size_t sql_len,
                             char *col_out, size_t col_len);
int rust_is_related_items_query(const char *pg_sql);
int rust_should_mask_collection_metadata_type(const char *pg_sql, const char *col_name, long long raw_val);
int rust_prepare_query_loop_tick(const char *sql, int *count_out, uint64_t *elapsed_ms_out);
char *rust_maybe_alias_collection_sync_aggregates(const char *sqlite_sql, const char *pg_sql);
void rust_free_cstring(char *ptr);
char *rust_simplify_fts_for_sqlite(const char *sql);
int rust_trace_prepare_sql_ok(const char *sql);
char *rust_strip_collate_icu_root(const char *sql);
char *rust_add_if_not_exists_for_sqlite_ddl(const char *sql);
normalized_sql_t* rust_normalize_sql_literals(const char *sql);
void rust_free_normalized_sql(normalized_sql_t *n);
int rust_is_junk_metadata_insert(const char *sql);
int rust_contains_binary_bytes(const unsigned char *data, size_t len);
char *rust_bytes_to_pg_hex(const unsigned char *data, size_t len);
int rust_is_library_db_path(const char *path);
int rust_is_library_or_blobs_db_path(const char *path);
int rust_is_blobs_db_path(const char *path);
int rust_find_insert_column_index(const char *sql, const char *column_name);
int rust_pg_oid_to_sqlite_type(unsigned int oid);
int rust_pg_text_to_int(const char *value);
long long rust_pg_text_to_int64(const char *value);
double rust_pg_text_to_double(const char *value);

/* pg_logging */
void rust_logging_init(void);
int rust_logging_get_level(void);
void rust_logging_write(int level, const char *message);
void rust_logging_fallback(const char *original_sql, const char *translated_sql,
                           const char *error_msg, const char *context);
int rust_logging_is_known_limitation(const char *error_msg);
void rust_logging_reset_after_fork(void);
void rust_logging_cleanup(void);

/* pg_mem_telemetry */
int rust_mem_telemetry_enabled(void);
void rust_mem_telemetry_add(int counter, unsigned long long bytes, unsigned long long events);
void rust_mem_telemetry_maybe_log(void);

/* pg_query_cache */
uint64_t rust_fnv1a_hash(const void *data, size_t len);
uint64_t rust_get_time_ms(void);
void rust_query_cache_init(void);
void rust_query_cache_cleanup(void);
uint64_t rust_query_cache_key(const char *pg_sql, const char *const *param_values, int param_count);
cached_result_t* rust_query_cache_lookup(uint64_t cache_key);
void rust_query_cache_store(uint64_t cache_key,
                            int num_rows,
                            int num_cols,
                            const Oid *col_types,
                            const char *const *col_names,
                            const char *const *values,
                            const int *lengths,
                            const int *is_null,
                            const char *pg_sql);
void rust_query_cache_invalidate(uint64_t cache_key);
void rust_query_cache_release(cached_result_t *entry);
void rust_query_cache_stats(uint64_t *hits, uint64_t *misses);

/* pg_statement */
int rust_oid_to_sqlite_type(unsigned int oid);
const char* rust_oid_to_sqlite_decltype(unsigned int oid);
int rust_decltype_special_case(unsigned int oid, const char *col_name, const char *pg_sql, unsigned int table_oid);
char* rust_convert_metadata_settings_upsert(const char *sql);
long long rust_extract_metadata_id(const char *sql);
void rust_stmt_set_callbacks(void (*ref_cb)(size_t),
                             void (*unref_cb)(size_t),
                             void (*free_cb)(size_t));
void rust_stmt_registry_init(void);
void rust_stmt_registry_cleanup(void);
void rust_stmt_register(size_t sqlite_stmt, size_t pg_stmt);
void rust_stmt_unregister(size_t sqlite_stmt);
size_t rust_stmt_find(size_t sqlite_stmt);
size_t rust_stmt_find_any(size_t sqlite_stmt);
int rust_stmt_is_ours(size_t pg_stmt);
void rust_cached_stmt_register(size_t sqlite_stmt, size_t pg_stmt);
size_t rust_cached_stmt_find(size_t sqlite_stmt);
void rust_cached_stmt_clear(size_t sqlite_stmt);
void rust_cached_stmt_clear_weak(size_t sqlite_stmt);
size_t* rust_cached_stmt_drain_all(int *count_out);
pg_value_t* rust_create_column_value(size_t stmt, int col_idx, int sqlite_type);
int rust_is_our_value(const pg_value_t *val);

/* pg_client */
uint64_t rust_hash_sql(const char *sql);
int rust_is_stale_sqlstate(const char *sqlstate);
int rust_is_duplicate_sqlstate(const char *sqlstate);
void rust_pool_set_callbacks(void* (*create_conn)(const char*),
                             void (*destroy_conn)(void*),
                             int (*check_conn_ok)(void*),
                             int (*reset_conn)(void*),
                             int (*reconnect_slot)(void*),
                             int (*get_txn_status)(void*),
                             int (*exec_simple)(void*, const char*),
                             int (*is_streaming_active)(void*),
                             int (*is_pg_active)(void*),
                             void (*set_pg_active)(void*, int),
                             int (*check_thread_alive)(uint64_t),
                             void (*stmt_cache_clear)(void*),
                             void (*get_db_path)(void*, char*, size_t),
                             uint64_t (*get_current_thread)(void),
                             int (*threads_equal)(uint64_t, uint64_t),
                             void (*sleep_ms)(int),
                             void (*get_retry_delays)(int*, int*),
                             void (*log_info)(const char*),
                             void (*log_error)(const char*),
                             void (*log_debug)(const char*));
void rust_pool_init(int pool_size, int idle_timeout);
void rust_pool_cleanup(void);
void* rust_pool_get_connection(const char *db_path);
void rust_pool_release_for_db(const void *db);
int rust_pool_validate_connection(const void *conn);
void rust_pool_touch_connection(const void *conn);
int rust_pool_check_health(void *conn);
void rust_pool_cleanup_after_fork(void);
void rust_register_connection(const void *db_handle, const void *conn);
void rust_unregister_connection(const void *db_handle);
void* rust_find_registered_connection(const void *db_handle);
void* rust_pool_find_connection(const void *db_handle, const char *db_path);
void* rust_find_any_library_connection(void);
int64_t rust_get_global_metadata_id(void);
void rust_set_global_metadata_id(int64_t id);
int64_t rust_get_global_last_insert_rowid(void);
void rust_set_global_last_insert_rowid(int64_t id);
int rust_pool_check_fork(void);

#endif
