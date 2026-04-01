//! Safe read accessors for the `orig_sqlite3_*` function pointers.
//!
//! Each `orig_sqlite3_*` global is a `static mut Option<fn(...)>` that is
//! written exactly once during shim initialisation (`.init_array` /
//! `__attribute__((constructor))`) and never mutated again.  These accessors
//! encapsulate the `unsafe` read so that call-sites only need `unsafe` for the
//! actual FFI call, which is the correct semantic boundary.

use super::state::*;
use super::*;

/// Generate a safe accessor that reads a `static mut Option<FnTy>`.
///
/// # Safety contract (upheld by the shim lifecycle)
/// The static is written once during init and is read-only afterwards, so
/// `ptr::read(addr_of!(...))` is sound from any thread after init completes.
macro_rules! orig_fn_accessor {
    ($accessor:ident, $static_name:ident, $ty:ty) => {
        /// Read the original sqlite3 function pointer (set once at init, read-only after).
        #[inline]
        #[allow(dead_code)]
        pub(crate) fn $accessor() -> Option<$ty> {
            // Safety: written once during shim init, read-only after.
            unsafe { std::ptr::read(std::ptr::addr_of!($static_name)) }
        }
    };
}

// --- open / close ---
orig_fn_accessor!(get_orig_sqlite3_open, orig_sqlite3_open, Sqlite3OpenFn);
orig_fn_accessor!(
    get_orig_sqlite3_open_v2,
    orig_sqlite3_open_v2,
    Sqlite3OpenV2Fn
);
orig_fn_accessor!(get_orig_sqlite3_close, orig_sqlite3_close, Sqlite3DbToIntFn);
orig_fn_accessor!(
    get_orig_sqlite3_close_v2,
    orig_sqlite3_close_v2,
    Sqlite3DbToIntFn
);

// --- exec / changes / rowid / get_table ---
orig_fn_accessor!(get_orig_sqlite3_exec, orig_sqlite3_exec, Sqlite3ExecFn);
orig_fn_accessor!(
    get_orig_sqlite3_changes,
    orig_sqlite3_changes,
    Sqlite3DbToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_changes64,
    orig_sqlite3_changes64,
    Sqlite3DbToI64Fn
);
orig_fn_accessor!(
    get_orig_sqlite3_last_insert_rowid,
    orig_sqlite3_last_insert_rowid,
    Sqlite3DbToI64Fn
);
orig_fn_accessor!(
    get_orig_sqlite3_get_table,
    orig_sqlite3_get_table,
    Sqlite3GetTableFn
);

// --- error info ---
orig_fn_accessor!(
    get_orig_sqlite3_errmsg,
    orig_sqlite3_errmsg,
    Sqlite3DbToCStrFn
);
orig_fn_accessor!(
    get_orig_sqlite3_errcode,
    orig_sqlite3_errcode,
    Sqlite3DbToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_extended_errcode,
    orig_sqlite3_extended_errcode,
    Sqlite3DbToIntFn
);

// --- prepare ---
orig_fn_accessor!(
    get_orig_sqlite3_prepare,
    orig_sqlite3_prepare,
    Sqlite3PrepareFn
);
orig_fn_accessor!(
    get_orig_sqlite3_prepare_v2,
    orig_sqlite3_prepare_v2,
    Sqlite3PrepareFn
);
orig_fn_accessor!(
    get_orig_sqlite3_prepare_v3,
    orig_sqlite3_prepare_v3,
    Sqlite3PrepareV3Fn
);
orig_fn_accessor!(
    get_orig_sqlite3_prepare16_v2,
    orig_sqlite3_prepare16_v2,
    Sqlite3Prepare16Fn
);

// --- bind ---
orig_fn_accessor!(
    get_orig_sqlite3_bind_int,
    orig_sqlite3_bind_int,
    Sqlite3BindIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_int64,
    orig_sqlite3_bind_int64,
    Sqlite3BindInt64Fn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_double,
    orig_sqlite3_bind_double,
    Sqlite3BindDoubleFn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_text,
    orig_sqlite3_bind_text,
    Sqlite3BindTextFn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_text64,
    orig_sqlite3_bind_text64,
    Sqlite3BindText64Fn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_blob,
    orig_sqlite3_bind_blob,
    Sqlite3BindBlobFn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_blob64,
    orig_sqlite3_bind_blob64,
    Sqlite3BindBlob64Fn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_value,
    orig_sqlite3_bind_value,
    Sqlite3BindValueFn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_null,
    orig_sqlite3_bind_null,
    Sqlite3BindNullFn
);

// --- step / reset / finalize / clear_bindings ---
orig_fn_accessor!(get_orig_sqlite3_step, orig_sqlite3_step, Sqlite3StmtToIntFn);
orig_fn_accessor!(
    get_orig_sqlite3_reset,
    orig_sqlite3_reset,
    Sqlite3StmtToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_finalize,
    orig_sqlite3_finalize,
    Sqlite3StmtToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_clear_bindings,
    orig_sqlite3_clear_bindings,
    Sqlite3StmtToIntFn
);

// --- column accessors ---
orig_fn_accessor!(
    get_orig_sqlite3_column_count,
    orig_sqlite3_column_count,
    Sqlite3StmtToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_type,
    orig_sqlite3_column_type,
    Sqlite3StmtIndexToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_int,
    orig_sqlite3_column_int,
    Sqlite3StmtIndexToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_int64,
    orig_sqlite3_column_int64,
    Sqlite3StmtIndexToI64Fn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_double,
    orig_sqlite3_column_double,
    Sqlite3StmtIndexToDoubleFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_text,
    orig_sqlite3_column_text,
    Sqlite3StmtIndexToTextFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_blob,
    orig_sqlite3_column_blob,
    Sqlite3StmtIndexToBlobFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_bytes,
    orig_sqlite3_column_bytes,
    Sqlite3StmtIndexToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_name,
    orig_sqlite3_column_name,
    Sqlite3StmtIndexToNameFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_decltype,
    orig_sqlite3_column_decltype,
    Sqlite3StmtIndexToNameFn
);
orig_fn_accessor!(
    get_orig_sqlite3_column_value,
    orig_sqlite3_column_value,
    Sqlite3StmtIndexToValueFn
);
orig_fn_accessor!(
    get_orig_sqlite3_data_count,
    orig_sqlite3_data_count,
    Sqlite3StmtToIntFn
);

// --- value accessors ---
orig_fn_accessor!(
    get_orig_sqlite3_value_type,
    orig_sqlite3_value_type,
    Sqlite3ValueToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_value_text,
    orig_sqlite3_value_text,
    Sqlite3ValueToTextFn
);
orig_fn_accessor!(
    get_orig_sqlite3_value_int,
    orig_sqlite3_value_int,
    Sqlite3ValueToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_value_int64,
    orig_sqlite3_value_int64,
    Sqlite3ValueToI64Fn
);
orig_fn_accessor!(
    get_orig_sqlite3_value_double,
    orig_sqlite3_value_double,
    Sqlite3ValueToDoubleFn
);
orig_fn_accessor!(
    get_orig_sqlite3_value_bytes,
    orig_sqlite3_value_bytes,
    Sqlite3ValueToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_value_blob,
    orig_sqlite3_value_blob,
    Sqlite3ValueToBlobFn
);

// --- collation ---
orig_fn_accessor!(
    get_orig_sqlite3_create_collation,
    orig_sqlite3_create_collation,
    Sqlite3CreateCollationFn
);
orig_fn_accessor!(
    get_orig_sqlite3_create_collation_v2,
    orig_sqlite3_create_collation_v2,
    Sqlite3CreateCollationV2Fn
);

// --- memory ---
orig_fn_accessor!(get_orig_sqlite3_free, orig_sqlite3_free, Sqlite3FreeFn);
orig_fn_accessor!(
    get_orig_sqlite3_malloc,
    orig_sqlite3_malloc,
    Sqlite3MallocFn
);

// --- stmt utilities ---
orig_fn_accessor!(
    get_orig_sqlite3_db_handle,
    orig_sqlite3_db_handle,
    Sqlite3StmtToDbFn
);
orig_fn_accessor!(get_orig_sqlite3_sql, orig_sqlite3_sql, Sqlite3StmtToCStrFn);
orig_fn_accessor!(
    get_orig_sqlite3_expanded_sql,
    orig_sqlite3_expanded_sql,
    Sqlite3StmtToMutCStrFn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_parameter_count,
    orig_sqlite3_bind_parameter_count,
    Sqlite3StmtToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_parameter_index,
    orig_sqlite3_bind_parameter_index,
    Sqlite3StmtNameToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_stmt_readonly,
    orig_sqlite3_stmt_readonly,
    Sqlite3StmtToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_stmt_busy,
    orig_sqlite3_stmt_busy,
    Sqlite3StmtToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_stmt_status,
    orig_sqlite3_stmt_status,
    Sqlite3StmtIdx2ToIntFn
);
orig_fn_accessor!(
    get_orig_sqlite3_bind_parameter_name,
    orig_sqlite3_bind_parameter_name,
    Sqlite3StmtIndexToNameFn
);

// --- shim pointers ---
orig_fn_accessor!(
    get_shim_sqlite3_prepare_v2,
    shim_sqlite3_prepare_v2,
    Sqlite3PrepareFn
);
orig_fn_accessor!(
    get_shim_sqlite3_errmsg,
    shim_sqlite3_errmsg,
    Sqlite3DbToCStrFn
);
orig_fn_accessor!(
    get_shim_sqlite3_errcode,
    shim_sqlite3_errcode,
    Sqlite3DbToIntFn
);
