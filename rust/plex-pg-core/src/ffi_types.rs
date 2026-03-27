use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::AtomicI32;

use crate::libpq_helpers::{PGconn, PGresult};
use crate::pg_query_cache::CachedResult;

pub const MAX_PARAMS: usize = 128;
pub const MAX_COLS: usize = 128;
pub const STMT_CACHE_SIZE: usize = 512;
pub const DB_PATH_LEN: usize = 1024;
pub const LAST_ERROR_LEN: usize = 1024;
pub const STMT_NAME_LEN: usize = 32;
pub const PARAM_BUF_LEN: usize = 32;

#[repr(C)]
pub struct sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
pub struct sqlite3_stmt {
    _private: [u8; 0],
}

#[repr(C)]
pub struct sqlite3_value {
    _private: [u8; 0],
}

#[repr(C)]
pub struct PgConnection {
    pub conn: *mut PGconn,
    pub shadow_db: *mut sqlite3,
    pub db_path: [c_char; DB_PATH_LEN],
    pub is_pg_active: c_int,
    pub in_transaction: c_int,
    pub mutex: libc::pthread_mutex_t,
    pub last_changes: c_int,
    pub last_insert_rowid: i64,
    pub last_generator_metadata_id: i64,
    pub last_error: [c_char; LAST_ERROR_LEN],
    pub last_error_code: c_int,
    pub streaming_active: AtomicI32,
}

#[repr(C)]
pub struct PgStmt {
    pub mutex: libc::pthread_mutex_t,
    pub ref_count: AtomicI32,
    pub conn: *mut PgConnection,
    pub shadow_stmt: *mut sqlite3_stmt,
    pub sql: *mut c_char,
    pub pg_sql: *mut c_char,
    pub result: *mut PGresult,
    pub cached_result: *mut CachedResult,
    pub sql_hash: u64,
    pub stmt_name: [c_char; STMT_NAME_LEN],
    pub use_prepared: c_int,
    pub current_row: c_int,
    pub num_rows: c_int,
    pub num_cols: c_int,
    pub is_pg: c_int,
    pub is_cached: c_int,
    pub is_count_query: c_int,
    pub needs_requery: c_int,
    pub write_executed: c_int,
    pub read_done: c_int,
    pub metadata_only_result: c_int,
    pub in_step: AtomicI32,
    pub executing_thread: libc::pthread_t,
    pub result_conn: *mut PgConnection,
    pub col_names: *mut *mut c_char,
    pub num_col_names: c_int,
    pub streaming_mode: c_int,
    pub streaming_conn: *mut PgConnection,
    pub param_values: [*mut c_char; MAX_PARAMS],
    pub param_lengths: [c_int; MAX_PARAMS],
    pub param_formats: [c_int; MAX_PARAMS],
    pub param_count: c_int,
    pub param_names: *mut *mut c_char,
    pub param_buffers: [[c_char; PARAM_BUF_LEN]; MAX_PARAMS],
    pub decoded_blobs: [*mut c_void; MAX_PARAMS],
    pub decoded_blob_lens: [c_int; MAX_PARAMS],
    pub decoded_blob_row: c_int,
    pub cached_text: [*mut c_char; MAX_PARAMS],
    pub cached_blob: [*mut c_void; MAX_PARAMS],
    pub cached_blob_len: [c_int; MAX_PARAMS],
    pub cached_row: c_int,
    pub col_table_names: [*mut c_char; MAX_COLS],
    pub col_tables_resolved: c_int,
}
