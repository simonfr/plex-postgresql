/// Module: ffi
///
/// C-callable interface for the plex-pg-core crate.
/// Exposed symbols:
///   sql_translator_translate()       — translate SQLite SQL, return C string or NULL
///   sql_translator_free()            — free a string returned by translate()
///   sql_translator_last_error()      — return last error message for this thread
///   sql_translator_translate_full()  — translate and return a full SqlTranslation struct
///   sql_translator_translation_free()— free sql + param_names inside a SqlTranslation
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

// ─── Thread-local error storage ───────────────────────────────────────────────

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = RefCell::new(None);
}

fn set_last_error(msg: &str) {
    // Replace any interior NUL bytes so CString::new() never panics.
    let safe = msg.replace('\0', "<NUL>");
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = CString::new(safe).ok();
    });
}

// ─── Simple translate / free ──────────────────────────────────────────────────

/// Translate SQLite SQL to PostgreSQL SQL.
///
/// Returns a heap-allocated C string on success.  The caller **must** free the
/// returned pointer with `sql_translator_free()`.  Returns NULL on failure; the
/// error description can be retrieved with `sql_translator_last_error()`.
#[no_mangle]
pub extern "C" fn sql_translator_translate(sql: *const c_char) -> *mut c_char {
    if sql.is_null() {
        set_last_error("sql_translator_translate: received NULL pointer");
        return std::ptr::null_mut();
    }

    let sql_str = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_last_error(&format!("invalid UTF-8 input: {e}"));
            return std::ptr::null_mut();
        }
    };

    match crate::translate(sql_str) {
        Ok(t) => match CString::new(t.sql) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(&format!("translated SQL contains interior NUL: {e}"));
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            set_last_error(&e);
            std::ptr::null_mut()
        }
    }
}

/// Free a C string previously returned by `sql_translator_translate()`.
/// Calling with NULL is safe and a no-op.
#[no_mangle]
pub extern "C" fn sql_translator_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(ptr));
    }
}

/// Return the last error message recorded for this thread.
///
/// The returned pointer is valid until the next call to any `sql_translator_*`
/// function on this thread.  The caller must **not** free the pointer.
/// Returns NULL if no error has been recorded yet.
#[no_mangle]
pub extern "C" fn sql_translator_last_error() -> *const c_char {
    LAST_ERROR.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|cs| cs.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

// ─── Full struct API ──────────────────────────────────────────────────────────

/// Full translation result returned by `sql_translator_translate_full()`.
///
/// - `sql`         — heap-allocated C string; free with `sql_translator_free()`.
/// - `param_count` — number of bind parameters (`$1`, `$2`, …).
/// - `param_names` — heap-allocated array of `param_count` C strings (or NULLs).
///                   For `?` placeholders the entry is NULL; for `:name` it's the name.
///                   The array AND each non-NULL string must be freed by the caller
///                   (or use `sql_translator_translation_free()`).
/// - `success`     — 1 on success, 0 on error.
/// - `error`       — null-terminated error message (empty string on success).
#[repr(C)]
pub struct SqlTranslation {
    pub sql: *mut c_char,
    pub param_count: i32,
    pub param_names: *mut *mut c_char,
    pub success: i32,
    pub error: [u8; 256],
}

/// Translate SQLite SQL and return a full `SqlTranslation` struct.
///
/// On success `success` is 1 and `sql` points to a heap-allocated C string.
/// `param_names` is a heap-allocated array of `param_count` pointers:
///   - NULL for positional `?` placeholders
///   - heap-allocated C string for named `:name` placeholders
///
/// Free everything with `sql_translator_translation_free()`.
#[no_mangle]
pub extern "C" fn sql_translator_translate_full(sql: *const c_char) -> SqlTranslation {
    let mut result = SqlTranslation {
        sql: std::ptr::null_mut(),
        param_count: 0,
        param_names: std::ptr::null_mut(),
        success: 0,
        error: [0u8; 256],
    };

    let write_error = |buf: &mut [u8; 256], msg: &str| {
        let bytes = msg.as_bytes();
        let len = bytes.len().min(buf.len() - 1);
        buf[..len].copy_from_slice(&bytes[..len]);
        buf[len] = 0;
    };

    if sql.is_null() {
        write_error(
            &mut result.error,
            "sql_translator_translate_full: NULL input",
        );
        return result;
    }

    let sql_str = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            write_error(&mut result.error, &format!("invalid UTF-8: {e}"));
            return result;
        }
    };

    match crate::translate(sql_str) {
        Ok(t) => {
            let count = t.param_names.len();
            result.param_count = count as i32;

            // Build param_names C array
            if count > 0 {
                let layout = std::alloc::Layout::array::<*mut c_char>(count).unwrap();
                let arr = unsafe { std::alloc::alloc(layout) as *mut *mut c_char };
                if !arr.is_null() {
                    for (i, name) in t.param_names.iter().enumerate() {
                        let ptr = match name {
                            Some(n) => CString::new(n.as_str())
                                .map(|cs| cs.into_raw())
                                .unwrap_or(std::ptr::null_mut()),
                            None => std::ptr::null_mut(),
                        };
                        unsafe {
                            *arr.add(i) = ptr;
                        }
                    }
                    result.param_names = arr;
                }
            }

            match CString::new(t.sql) {
                Ok(cs) => {
                    result.sql = cs.into_raw();
                    result.success = 1;
                }
                Err(e) => {
                    write_error(
                        &mut result.error,
                        &format!("translated SQL contains NUL: {e}"),
                    );
                    // Clean up param_names on failure
                    free_param_names(result.param_names, count);
                    result.param_names = std::ptr::null_mut();
                }
            }
        }
        Err(e) => {
            write_error(&mut result.error, &e);
        }
    }

    result
}

/// Free the param_names array and its contents.
fn free_param_names(arr: *mut *mut c_char, count: usize) {
    if arr.is_null() {
        return;
    }
    for i in 0..count {
        let ptr = unsafe { *arr.add(i) };
        if !ptr.is_null() {
            unsafe {
                drop(CString::from_raw(ptr));
            }
        }
    }
    let layout = std::alloc::Layout::array::<*mut c_char>(count).unwrap();
    unsafe {
        std::alloc::dealloc(arr as *mut u8, layout);
    }
}

/// Free all heap-allocated fields inside a `SqlTranslation`.
///
/// Frees `sql`, each non-NULL entry in `param_names`, and the `param_names`
/// array itself.  Passing NULL is safe and a no-op.
#[no_mangle]
pub extern "C" fn sql_translator_translation_free(t: *mut SqlTranslation) {
    if t.is_null() {
        return;
    }
    unsafe {
        let trans = &mut *t;
        if !trans.sql.is_null() {
            drop(CString::from_raw(trans.sql));
            trans.sql = std::ptr::null_mut();
        }
        free_param_names(trans.param_names, trans.param_count as usize);
        trans.param_names = std::ptr::null_mut();
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn ffi_basic_translation() {
        let sql = CString::new("SELECT id FROM t WHERE id = ?").unwrap();
        let result = sql_translator_translate(sql.as_ptr());
        assert!(!result.is_null());
        let s = unsafe { CStr::from_ptr(result).to_string_lossy().into_owned() };
        assert!(s.contains("$1"));
        sql_translator_free(result);
    }

    #[test]
    fn ffi_null_input_returns_null() {
        let result = sql_translator_translate(std::ptr::null());
        assert!(result.is_null());
        let err = sql_translator_last_error();
        assert!(!err.is_null());
    }

    #[test]
    fn ffi_free_null_is_safe() {
        sql_translator_free(std::ptr::null_mut());
    }

    #[test]
    fn ffi_full_struct_param_count() {
        let sql = CString::new("SELECT * FROM t WHERE a = ? AND b = ?").unwrap();
        let result = sql_translator_translate_full(sql.as_ptr());
        assert_eq!(result.success, 1);
        assert_eq!(result.param_count, 2);
        assert!(!result.sql.is_null());
        let s = unsafe { CStr::from_ptr(result.sql).to_string_lossy().into_owned() };
        assert!(s.contains("$1") && s.contains("$2"));
        // param_names should be non-null with 2 entries (both NULL for ?)
        assert!(!result.param_names.is_null());
        unsafe {
            assert!((*result.param_names.add(0)).is_null()); // ? → NULL
            assert!((*result.param_names.add(1)).is_null()); // ? → NULL
        }
        sql_translator_free(result.sql);
        free_param_names(result.param_names, result.param_count as usize);
    }

    #[test]
    fn ffi_full_struct_named_params() {
        let sql = CString::new("SELECT * FROM t WHERE a = :foo AND b = :bar AND c = :foo").unwrap();
        let result = sql_translator_translate_full(sql.as_ptr());
        assert_eq!(result.success, 1);
        // :foo and :bar → 2 unique params (but :foo reuses $1)
        assert_eq!(result.param_count, 2);
        assert!(!result.param_names.is_null());
        unsafe {
            let p0 = *result.param_names.add(0);
            let p1 = *result.param_names.add(1);
            assert!(!p0.is_null());
            assert!(!p1.is_null());
            let n0 = CStr::from_ptr(p0).to_string_lossy();
            let n1 = CStr::from_ptr(p1).to_string_lossy();
            assert_eq!(n0, "foo");
            assert_eq!(n1, "bar");
        }
        let mut result = result;
        sql_translator_translation_free(&mut result);
    }
}
