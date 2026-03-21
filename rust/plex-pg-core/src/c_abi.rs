use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_void};

use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value, PgConnection, PgStmt};
use crate::libpq_helpers::PGresult;
use crate::pg_query_cache::CachedResult;

type CollationCompare =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int>;
type CollationDestroy = Option<unsafe extern "C" fn(*mut c_void)>;

// ─── sqlite3 interpose entrypoints ────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn my_sqlite3_open(filename: *const c_char, pp_db: *mut *mut sqlite3) -> c_int {
    crate::db_interpose_open::rust_my_sqlite3_open(filename, pp_db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_open_v2(
    filename: *const c_char,
    pp_db: *mut *mut sqlite3,
    flags: c_int,
    z_vfs: *const c_char,
) -> c_int {
    crate::db_interpose_open::rust_my_sqlite3_open_v2(filename, pp_db, flags, z_vfs)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_close(db: *mut sqlite3) -> c_int {
    crate::db_interpose_open::rust_my_sqlite3_close(db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_close_v2(db: *mut sqlite3) -> c_int {
    crate::db_interpose_open::rust_my_sqlite3_close_v2(db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    crate::db_interpose_exec::rust_my_sqlite3_exec(db, sql, callback, arg, errmsg)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_prepare_v2_internal(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
    from_worker: c_int,
) -> c_int {
    crate::db_interpose_prepare::rust_my_sqlite3_prepare_v2_internal(
        db, z_sql, n_byte, pp_stmt, pz_tail, from_worker,
    )
}

#[no_mangle]
pub extern "C" fn my_sqlite3_prepare(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    crate::db_interpose_prepare::rust_my_sqlite3_prepare(db, z_sql, n_byte, pp_stmt, pz_tail)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_prepare_v2(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    crate::db_interpose_prepare::rust_my_sqlite3_prepare_v2(db, z_sql, n_byte, pp_stmt, pz_tail)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_prepare_v3(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    prep_flags: c_uint,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    crate::db_interpose_prepare::rust_my_sqlite3_prepare_v3(
        db, z_sql, n_byte, prep_flags, pp_stmt, pz_tail,
    )
}

#[no_mangle]
pub extern "C" fn my_sqlite3_prepare16_v2(
    db: *mut sqlite3,
    z_sql: *const c_void,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_void,
) -> c_int {
    crate::db_interpose_prepare::rust_my_sqlite3_prepare16_v2(
        db, z_sql, n_byte, pp_stmt, pz_tail,
    )
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_int(p_stmt: *mut sqlite3_stmt, idx: c_int, val: c_int) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_int(p_stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_int64(p_stmt: *mut sqlite3_stmt, idx: c_int, val: i64) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_int64(p_stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_double(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: f64,
) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_double(p_stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_text(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_text(p_stmt, idx, val, n_bytes, destructor)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_text64(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n_bytes: u64,
    destructor: *mut c_void,
    encoding: c_uchar,
) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_text64(
        p_stmt, idx, val, n_bytes, destructor, encoding,
    )
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_blob(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: c_int,
    destructor: *mut c_void,
) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_blob(p_stmt, idx, val, n_bytes, destructor)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_blob64(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n_bytes: u64,
    destructor: *mut c_void,
) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_blob64(p_stmt, idx, val, n_bytes, destructor)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_value(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
    value: *const sqlite3_value,
) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_value(p_stmt, idx, value)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_null(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    crate::db_interpose_bind::rust_my_sqlite3_bind_null(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_step(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_step::rust_my_sqlite3_step(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_reset(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_stmt_lifecycle::rust_my_sqlite3_reset(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_finalize(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_stmt_lifecycle::rust_my_sqlite3_finalize(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_clear_bindings(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_stmt_lifecycle::rust_my_sqlite3_clear_bindings(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_column::rust_my_sqlite3_column_count(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_type(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    crate::db_interpose_column::rust_my_sqlite3_column_type(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_int(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    crate::db_interpose_column::rust_my_sqlite3_column_int(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_int64(p_stmt: *mut sqlite3_stmt, idx: c_int) -> i64 {
    crate::db_interpose_column::rust_my_sqlite3_column_int64(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_double(p_stmt: *mut sqlite3_stmt, idx: c_int) -> f64 {
    crate::db_interpose_column::rust_my_sqlite3_column_double(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_text(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_uchar {
    crate::db_interpose_column::rust_my_sqlite3_column_text(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_blob(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_void {
    crate::db_interpose_column::rust_my_sqlite3_column_blob(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_bytes(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    crate::db_interpose_column::rust_my_sqlite3_column_bytes(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_name(p_stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    crate::db_interpose_column::rust_my_sqlite3_column_name(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_decltype(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    crate::db_interpose_column::rust_my_sqlite3_column_decltype(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_column_value(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *mut sqlite3_value {
    crate::db_interpose_column::rust_my_sqlite3_column_value(p_stmt, idx)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_data_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_column::rust_my_sqlite3_data_count(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_value_type(p_val: *mut sqlite3_value) -> c_int {
    crate::db_interpose_value::rust_my_sqlite3_value_type(p_val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_value_text(p_val: *mut sqlite3_value) -> *const c_uchar {
    crate::db_interpose_value::rust_my_sqlite3_value_text(p_val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_value_int(p_val: *mut sqlite3_value) -> c_int {
    crate::db_interpose_value::rust_my_sqlite3_value_int(p_val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_value_int64(p_val: *mut sqlite3_value) -> i64 {
    crate::db_interpose_value::rust_my_sqlite3_value_int64(p_val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_value_double(p_val: *mut sqlite3_value) -> f64 {
    crate::db_interpose_value::rust_my_sqlite3_value_double(p_val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_value_bytes(p_val: *mut sqlite3_value) -> c_int {
    crate::db_interpose_value::rust_my_sqlite3_value_bytes(p_val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_value_blob(p_val: *mut sqlite3_value) -> *const c_void {
    crate::db_interpose_value::rust_my_sqlite3_value_blob(p_val)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_changes(db: *mut sqlite3) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_changes(db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_changes64(db: *mut sqlite3) -> i64 {
    crate::db_interpose_metadata::rust_my_sqlite3_changes64(db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64 {
    crate::db_interpose_metadata::rust_my_sqlite3_last_insert_rowid(db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_errmsg(db: *mut sqlite3) -> *const c_char {
    crate::db_interpose_metadata::rust_my_sqlite3_errmsg(db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_errcode(db: *mut sqlite3) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_errcode(db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_extended_errcode(db: *mut sqlite3) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_extended_errcode(db)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_get_table(
    db: *mut sqlite3,
    sql: *const c_char,
    paz_result: *mut *mut *mut c_char,
    pn_row: *mut c_int,
    pn_col: *mut c_int,
    pz_err_msg: *mut *mut c_char,
) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_get_table(
        db, sql, paz_result, pn_row, pn_col, pz_err_msg,
    )
}

#[no_mangle]
pub extern "C" fn my_sqlite3_create_collation(
    db: *mut sqlite3,
    name: *const c_char,
    text_rep: c_int,
    arg: *mut c_void,
    compare: CollationCompare,
) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_create_collation(
        db, name, text_rep, arg, compare,
    )
}

#[no_mangle]
pub extern "C" fn my_sqlite3_create_collation_v2(
    db: *mut sqlite3,
    name: *const c_char,
    text_rep: c_int,
    arg: *mut c_void,
    compare: CollationCompare,
    destroy: CollationDestroy,
) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_create_collation_v2(
        db, name, text_rep, arg, compare, destroy,
    )
}

#[no_mangle]
pub extern "C" fn my_sqlite3_free(ptr: *mut c_void) {
    crate::db_interpose_metadata::rust_my_sqlite3_free(ptr);
}

#[no_mangle]
pub extern "C" fn my_sqlite3_malloc(n: c_int) -> *mut c_void {
    crate::db_interpose_metadata::rust_my_sqlite3_malloc(n)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_db_handle(p_stmt: *mut sqlite3_stmt) -> *mut sqlite3 {
    crate::db_interpose_metadata::rust_my_sqlite3_db_handle(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_sql(p_stmt: *mut sqlite3_stmt) -> *const c_char {
    crate::db_interpose_metadata::rust_my_sqlite3_sql(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_expanded_sql(p_stmt: *mut sqlite3_stmt) -> *mut c_char {
    crate::db_interpose_metadata::rust_my_sqlite3_expanded_sql(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_parameter_count(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_bind_parameter_count(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_parameter_index(
    p_stmt: *mut sqlite3_stmt,
    name: *const c_char,
) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_bind_parameter_index(p_stmt, name)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_stmt_readonly(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_stmt_readonly(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_stmt_busy(p_stmt: *mut sqlite3_stmt) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_stmt_busy(p_stmt)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_stmt_status(
    p_stmt: *mut sqlite3_stmt,
    op: c_int,
    reset_flag: c_int,
) -> c_int {
    crate::db_interpose_metadata::rust_my_sqlite3_stmt_status(p_stmt, op, reset_flag)
}

#[no_mangle]
pub extern "C" fn my_sqlite3_bind_parameter_name(
    p_stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    crate::db_interpose_metadata::rust_my_sqlite3_bind_parameter_name(p_stmt, idx)
}

// ─── Non-sqlite3 helpers exported for C callers ──────────────────────────────

#[no_mangle]
pub extern "C" fn resolve_column_tables(pg_stmt: *mut PgStmt, pg_conn: *mut PgConnection) -> c_int {
    crate::db_interpose_column::rust_resolve_column_tables(pg_stmt, pg_conn)
}

#[no_mangle]
pub extern "C" fn pg_decode_bytea(
    pg_stmt: *mut PgStmt,
    row: c_int,
    col: c_int,
    out_length: *mut c_int,
) -> *const c_void {
    crate::db_interpose_column::rust_pg_decode_bytea_cached(pg_stmt, row, col, out_length)
}

#[no_mangle]
pub extern "C" fn pg_note_stmt_prepare(p_stmt: *mut sqlite3_stmt, sql: *const c_char) {
    crate::db_interpose_stmt_lifecycle::rust_pg_note_stmt_prepare(p_stmt, sql);
}

#[no_mangle]
pub extern "C" fn skip_leading_sql_noise(sql: *const c_char) -> *const c_char {
    crate::db_interpose_txn_utils::rust_skip_leading_sql_noise(sql)
}

#[no_mangle]
pub extern "C" fn is_txn_terminator_sql(sql: *const c_char) -> c_int {
    crate::db_interpose_txn_utils::rust_is_txn_terminator_sql(sql)
}

#[no_mangle]
pub extern "C" fn txn_terminator_should_noop(
    conn: *mut PgConnection,
    sql: *const c_char,
    txn_state_out: *mut c_int,
) -> c_int {
    crate::db_interpose_txn_utils::rust_txn_terminator_should_noop(conn, sql, txn_state_out)
}

#[no_mangle]
pub extern "C" fn step_conn_cancel_and_drain(conn: *mut PgConnection, scope_tag: *const c_char) {
    crate::db_interpose_conn_utils::rust_step_conn_cancel_and_drain(conn, scope_tag);
}

#[no_mangle]
pub extern "C" fn step_pick_thread_connection(base_conn: *mut PgConnection) -> *mut PgConnection {
    crate::db_interpose_step_write_utils::rust_step_pick_thread_connection(base_conn)
}

#[no_mangle]
pub extern "C" fn step_cached_write_should_noop(
    base_conn: *mut PgConnection,
    sql: *const c_char,
    out_exec_conn: *mut *mut PgConnection,
) -> c_int {
    crate::db_interpose_step_write_utils::rust_step_cached_write_should_noop(
        base_conn, sql, out_exec_conn,
    )
}

#[no_mangle]
pub extern "C" fn step_pg_write_should_noop(
    exec_conn: *mut PgConnection,
    pg_sql: *const c_char,
    txn_state_out: *mut c_int,
) -> c_int {
    crate::db_interpose_step_write_utils::rust_step_pg_write_should_noop(
        exec_conn, pg_sql, txn_state_out,
    )
}

#[no_mangle]
pub extern "C" fn step_cached_write_build_exec_sql(
    orig_sql: *const c_char,
    translated_sql: *const c_char,
    exec_sql_out: *mut *const c_char,
) -> *mut c_char {
    crate::db_interpose_step_write_utils::rust_step_cached_write_build_exec_sql(
        orig_sql, translated_sql, exec_sql_out,
    )
}

#[no_mangle]
pub extern "C" fn step_write_should_skip_special_insert(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
) -> c_int {
    crate::db_interpose_step_write_utils::rust_step_write_should_skip_special_insert(
        pg_stmt, exec_conn, param_values,
    )
}

#[no_mangle]
pub extern "C" fn step_write_prepare_connection(
    pg_stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    crate::db_interpose_step_write_utils::rust_step_write_prepare_connection(
        pg_stmt, exec_conn_io, pg_conn_error_out,
    )
}

#[no_mangle]
pub extern "C" fn step_write_execute_and_finalize(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    crate::db_interpose_step_write_utils::rust_step_write_execute_and_finalize(
        pg_stmt, exec_conn, param_values, pg_conn_error_out,
    )
}

#[no_mangle]
pub extern "C" fn step_cached_write_execute_and_finalize(
    cached_io: *mut *mut PgStmt,
    p_stmt: *mut sqlite3_stmt,
    changes_conn: *mut PgConnection,
    exec_conn: *mut PgConnection,
    orig_sql: *const c_char,
    exec_sql: *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    crate::db_interpose_step_write_utils::rust_step_cached_write_execute_and_finalize(
        cached_io,
        p_stmt,
        changes_conn,
        exec_conn,
        orig_sql,
        exec_sql,
        pg_conn_error_out,
    )
}

#[no_mangle]
pub extern "C" fn step_write_log_debug_context(
    pg_stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
    param_values: *const *const c_char,
) {
    crate::db_interpose_step_write_utils::rust_step_write_log_debug_context(
        pg_stmt, exec_conn, param_values,
    );
}

#[no_mangle]
pub extern "C" fn step_log_step_exit_trace(pg_stmt: *mut PgStmt) {
    crate::db_interpose_step_write_utils::rust_step_log_step_exit_trace(pg_stmt);
}

#[no_mangle]
pub extern "C" fn step_cached_read_finalize_advance(
    cached: *mut PgStmt,
    expanded_sql: *mut c_char,
    step_rc_out: *mut c_int,
) -> c_int {
    crate::db_interpose_step_cached_read_utils::rust_step_cached_read_finalize_advance(
        cached, expanded_sql, step_rc_out,
    )
}

#[no_mangle]
pub extern "C" fn step_cached_read_prepare_stmt(
    cached: *mut PgStmt,
    conn: *mut PgConnection,
    sql: *const c_char,
    p_stmt: *mut sqlite3_stmt,
    translated_sql: *const c_char,
) -> *mut PgStmt {
    crate::db_interpose_step_cached_read_utils::rust_step_cached_read_prepare_stmt(
        cached, conn, sql, p_stmt, translated_sql,
    )
}

#[no_mangle]
pub extern "C" fn step_cached_read_execute(
    stmt: *mut PgStmt,
    conn: *mut PgConnection,
    orig_sql: *const c_char,
    translated_sql: *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    crate::db_interpose_step_cached_read_utils::rust_step_cached_read_execute(
        stmt, conn, orig_sql, translated_sql, pg_conn_error_out,
    )
}

#[no_mangle]
pub extern "C" fn step_read_advance_cached_result(stmt: *mut PgStmt) -> c_int {
    crate::db_interpose_step_read_utils::rust_step_read_advance_cached_result(stmt)
}

#[no_mangle]
pub extern "C" fn step_read_streaming_next(p_stmt: *mut sqlite3_stmt, stmt: *mut PgStmt) -> c_int {
    crate::db_interpose_step_read_utils::rust_step_read_streaming_next(p_stmt, stmt)
}

#[no_mangle]
pub extern "C" fn step_read_eager_next(stmt: *mut PgStmt) -> c_int {
    crate::db_interpose_step_read_utils::rust_step_read_eager_next(stmt)
}

#[no_mangle]
pub extern "C" fn step_read_first_execute(
    stmt: *mut PgStmt,
    exec_conn_io: *mut *mut PgConnection,
    param_values: *const *const c_char,
    pg_conn_error_out: *mut c_int,
) -> c_int {
    crate::db_interpose_step_read_utils::rust_step_read_first_execute(
        stmt, exec_conn_io, param_values, pg_conn_error_out,
    )
}

#[no_mangle]
pub extern "C" fn step_read_log_debug_context(stmt: *mut PgStmt, exec_conn: *mut PgConnection) {
    crate::db_interpose_step_read_utils::rust_step_read_log_debug_context(stmt, exec_conn);
}

#[no_mangle]
pub extern "C" fn step_read_prepare_reexecution_state(
    stmt: *mut PgStmt,
    exec_conn: *mut PgConnection,
) {
    crate::db_interpose_step_read_utils::rust_step_read_prepare_reexecution_state(stmt, exec_conn);
}

// ─── Query cache C-ABI helpers ───────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn pg_query_cache_init() {
    crate::pg_query_cache::rust_query_cache_init();
}

#[no_mangle]
pub extern "C" fn pg_query_cache_cleanup() {
    crate::pg_query_cache::rust_query_cache_cleanup();
}

#[no_mangle]
pub extern "C" fn pg_query_cache_key(stmt: *mut PgStmt) -> u64 {
    if stmt.is_null() {
        return 0;
    }
    unsafe {
        crate::pg_query_cache::rust_query_cache_key(
            (*stmt).pg_sql,
            (*stmt).param_values.as_ptr() as *const *const c_char,
            (*stmt).param_count,
        )
    }
}

#[no_mangle]
pub extern "C" fn pg_query_cache_lookup(stmt: *mut PgStmt) -> *mut CachedResult {
    let key = pg_query_cache_key(stmt);
    if key == 0 {
        return std::ptr::null_mut();
    }
    crate::pg_query_cache::rust_query_cache_lookup(key)
}

#[no_mangle]
pub extern "C" fn pg_query_cache_store(stmt: *mut PgStmt, result_ptr: *mut c_void) {
    if stmt.is_null() || result_ptr.is_null() {
        return;
    }

    let result = result_ptr as *mut PGresult;
    let status = crate::libpq_helpers::rust_pq_result_status(result);
    if status != crate::libpq_helpers::PGRES_TUPLES_OK {
        return;
    }

    let num_rows = crate::libpq_helpers::rust_pq_ntuples(result);
    let num_cols = crate::libpq_helpers::rust_pq_nfields(result);
    if num_rows <= 0 || num_cols <= 0 {
        return;
    }

    let key = pg_query_cache_key(stmt);
    if key == 0 {
        return;
    }

    unsafe {
        crate::db_interpose_helpers::rust_query_cache_store_from_pgresult(
            key,
            result,
            num_rows,
            num_cols,
            (*stmt).pg_sql,
        );
    }
}

#[no_mangle]
pub extern "C" fn pg_query_cache_invalidate(stmt: *mut PgStmt) {
    let key = pg_query_cache_key(stmt);
    if key == 0 {
        return;
    }
    crate::pg_query_cache::rust_query_cache_invalidate(key);
}

#[no_mangle]
pub extern "C" fn pg_query_cache_stats(hits: *mut u64, misses: *mut u64) {
    crate::pg_query_cache::rust_query_cache_stats(hits, misses);
}

#[no_mangle]
pub extern "C" fn pg_query_cache_release(entry: *mut CachedResult) {
    crate::pg_query_cache::rust_query_cache_release(entry);
}

// ─── Portable string helpers (str_utils.c replacement) ───────────────────────

#[no_mangle]
pub extern "C" fn safe_strcasestr(haystack: *const c_char, needle: *const c_char) -> *mut c_char {
    if haystack.is_null() || needle.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        if *needle == 0 {
            return haystack as *mut c_char;
        }
    }

    let needle_len = unsafe { libc::strlen(needle) };
    if needle_len == 0 {
        return haystack as *mut c_char;
    }

    let mut p = haystack;
    unsafe {
        while *p != 0 {
            if libc::strncasecmp(p, needle, needle_len) == 0 {
                return p as *mut c_char;
            }
            p = p.add(1);
        }
    }
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn str_replace_nocase(
    str_ptr: *const c_char,
    old_ptr: *const c_char,
    new_ptr: *const c_char,
) -> *mut c_char {
    if str_ptr.is_null() || old_ptr.is_null() || new_ptr.is_null() {
        return std::ptr::null_mut();
    }

    let old_len = unsafe { libc::strlen(old_ptr) };
    if old_len == 0 {
        return unsafe { libc::strdup(str_ptr) };
    }

    let mut out = Vec::new();
    let mut p = str_ptr;
    loop {
        let match_ptr = safe_strcasestr(p, old_ptr);
        if match_ptr.is_null() {
            let tail_len = unsafe { libc::strlen(p) };
            if tail_len > 0 {
                let tail = unsafe { std::slice::from_raw_parts(p as *const u8, tail_len) };
                out.extend_from_slice(tail);
            }
            break;
        }

        let prefix_len = unsafe { match_ptr.offset_from(p) as usize };
        if prefix_len > 0 {
            let prefix = unsafe { std::slice::from_raw_parts(p as *const u8, prefix_len) };
            out.extend_from_slice(prefix);
        }

        let new_len = unsafe { libc::strlen(new_ptr) };
        if new_len > 0 {
            let new_bytes = unsafe { std::slice::from_raw_parts(new_ptr as *const u8, new_len) };
            out.extend_from_slice(new_bytes);
        }

        p = unsafe { match_ptr.add(old_len) };
    }

    out.push(0);
    unsafe {
        let buf = libc::malloc(out.len()) as *mut u8;
        if buf.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(out.as_ptr(), buf, out.len());
        buf as *mut c_char
    }
}
