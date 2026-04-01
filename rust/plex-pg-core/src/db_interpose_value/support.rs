use super::*;

pub(super) struct FakeValueContext {
    pub pg_stmt: *mut PgStmt,
    pub row: c_int,
    pub col: c_int,
}

pub(super) fn sqlite_type_name(t: c_int) -> &'static str {
    match t {
        SQLITE_INTEGER => "INTEGER",
        SQLITE_FLOAT => "FLOAT",
        SQLITE_TEXT => "TEXT",
        SQLITE_BLOB => "BLOB",
        SQLITE_NULL => "NULL",
        _ => "UNKNOWN",
    }
}

pub(super) fn helpers_result_ptr(result: *mut PgResultLibpq) -> *const PgResultHelpers {
    result as *const PgResultHelpers
}

pub(super) fn fake_value_thread_ok(fake: *const PgFakeValue) -> bool {
    if fake.is_null() {
        return false;
    }
    let f = unsafe { &*fake };
    unsafe { libc::pthread_equal(f.owner_thread, libc::pthread_self()) != 0 }
}

pub(super) unsafe fn load_fake_value_context(
    p_val: *mut sqlite3_value,
    label: &str,
) -> Option<FakeValueContext> {
    if p_val.is_null() {
        return None;
    }

    let fake = pg_check_fake_value(p_val);
    if fake.is_null() {
        return None;
    }
    let f = &*fake;
    if f.pg_stmt.is_null() {
        return None;
    }
    if !fake_value_thread_ok(fake) {
        log_error(&format!(
            "{}: fake value from different thread (stmt={:p})",
            label, f.pg_stmt
        ));
        return None;
    }

    Some(FakeValueContext {
        pg_stmt: f.pg_stmt as *mut PgStmt,
        row: f.row_idx,
        col: f.col_idx,
    })
}

pub(super) unsafe fn fake_value_has_result(ctx: &FakeValueContext) -> bool {
    let s = &*ctx.pg_stmt;
    !s.result.is_null()
        && ctx.row >= 0
        && ctx.row < s.num_rows
        && ctx.col >= 0
        && ctx.col < s.num_cols
}
