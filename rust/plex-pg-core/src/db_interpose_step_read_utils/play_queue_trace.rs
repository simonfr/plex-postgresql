use super::support::{bytes_preview, cstr_to_str, param_at};
use super::*;
use crate::log_info_lazy;

static TRACE_PLAY_QUEUE: AtomicI32 = AtomicI32::new(-1);

fn trace_play_queue_enabled() -> bool {
    let cached = TRACE_PLAY_QUEUE.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let key = CString::new("PLEX_PG_TRACE_PLAY_QUEUE").unwrap();
    let val = unsafe { libc::getenv(key.as_ptr()) };
    let enabled = crate::db_interpose_helpers::rust_env_truthy(val) != 0;
    TRACE_PLAY_QUEUE.store(if enabled { 1 } else { 0 }, Ordering::Relaxed);
    enabled
}

unsafe fn is_play_queue_stmt(pg_stmt: *mut PgStmt) -> bool {
    if pg_stmt.is_null() {
        return false;
    }
    let s = &*pg_stmt;
    let sql_bytes = cstr_bytes(s.sql);
    let pg_sql_bytes = cstr_bytes(s.pg_sql);
    contains_icase_bytes(sql_bytes, b"play_queue")
        || contains_icase_bytes(pg_sql_bytes, b"play_queue")
}

pub(crate) unsafe fn trace_play_queue_params(
    pg_stmt: *mut PgStmt,
    param_values: *const *const c_char,
    phase: &str,
) {
    if !trace_play_queue_enabled() || !is_play_queue_stmt(pg_stmt) {
        return;
    }
    let s = &*pg_stmt;
    let param_count = s.param_count;
    let count = if param_count > 0 {
        param_count as usize
    } else {
        0
    };
    let max_params = 16usize;
    let max_len = 256usize;
    log_info_lazy!(
        "PLAY_QUEUE TRACE {}: param_count={} sql={:.200}",
        phase,
        param_count,
        cstr_to_str(s.pg_sql)
    );
    if !s.sql.is_null() && s.sql != s.pg_sql {
        log_info_lazy!(
            "PLAY_QUEUE TRACE {}: sqlite_sql={:.200}",
            phase,
            cstr_to_str(s.sql)
        );
    }
    if count == 0 {
        log_info_lazy!("PLAY_QUEUE TRACE {} params: (none)", phase);
        return;
    }
    let mut parts: Vec<String> = Vec::with_capacity(count.min(max_params));
    for i in 0..count.min(max_params) {
        let val_ptr = param_at(param_values, i);
        let val_str = if val_ptr.is_null() {
            "NULL".to_string()
        } else {
            let bytes = CStr::from_ptr(val_ptr).to_bytes();
            let (preview, truncated, total_len) = bytes_preview(bytes, max_len);
            if truncated {
                format!("{}...(len={})", preview, total_len)
            } else {
                preview
            }
        };
        parts.push(format!("${}={}", i + 1, val_str));
    }
    log_info_lazy!("PLAY_QUEUE TRACE {} params: {}", phase, parts.join(", "));
    if count > max_params {
        log_info_lazy!(
            "PLAY_QUEUE TRACE {} params: truncated {} of {}",
            phase,
            max_params,
            count
        );
    }
}

pub(crate) unsafe fn trace_play_queue_result(
    pg_stmt: *mut PgStmt,
    result: *mut PGresult,
    phase: &str,
) {
    if !trace_play_queue_enabled() || !is_play_queue_stmt(pg_stmt) || result.is_null() {
        return;
    }
    let num_rows = crate::libpq_helpers::rust_pq_ntuples(result);
    let num_cols = crate::libpq_helpers::rust_pq_nfields(result);
    let max_rows = 5i32;
    let max_cols = 16i32;
    let max_len = 256usize;
    log_info_lazy!(
        "PLAY_QUEUE TRACE {} result: rows={} cols={}",
        phase,
        num_rows,
        num_cols
    );
    let rows = if num_rows > 0 { num_rows } else { 0 };
    let cols = if num_cols > 0 { num_cols } else { 0 };
    let row_cap = rows.min(max_rows);
    let col_cap = cols.min(max_cols);
    for r in 0..row_cap {
        let mut parts: Vec<String> = Vec::with_capacity(col_cap as usize);
        for c in 0..col_cap {
            let name_ptr = crate::db_interpose_helpers::rust_pg_result_col_name(result, c);
            let name = if name_ptr.is_null() {
                format!("col{}", c)
            } else {
                CStr::from_ptr(name_ptr).to_str().unwrap_or("?").to_string()
            };
            let mut buf = vec![0u8; max_len + 1];
            let len = crate::db_interpose_helpers::rust_pg_result_text_copy(
                result,
                r,
                c,
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            );
            let val = if len < 0 {
                "NULL".to_string()
            } else {
                let total_len = len as usize;
                let copy_len = total_len.min(buf.len().saturating_sub(1));
                let (preview, truncated, _) = bytes_preview(&buf[..copy_len], max_len);
                if truncated || total_len > copy_len {
                    format!("{}...(len={})", preview, total_len)
                } else {
                    preview
                }
            };
            parts.push(format!("{}={}", name, val));
        }
        log_info_lazy!("PLAY_QUEUE TRACE {} row {}: {}", phase, r, parts.join(", "));
    }
    if rows > row_cap {
        log_info_lazy!(
            "PLAY_QUEUE TRACE {} result: truncated rows {} of {}",
            phase,
            row_cap,
            rows
        );
    }
    if cols > col_cap {
        log_info_lazy!(
            "PLAY_QUEUE TRACE {} result: truncated cols {} of {}",
            phase,
            col_cap,
            cols
        );
    }
}
