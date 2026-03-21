#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn stack_protection_smoke_linux() {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_void};

    let shim_path = match std::env::var("PLEX_PG_SHIM_PATH") {
        Ok(p) if !p.is_empty() => p,
        _ => return,
    };

    let path = CString::new(shim_path).unwrap();
    unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
        if handle.is_null() {
            panic!("failed to load shim");
        }

        type OpenFn = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int;
        type PrepareFn = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            c_int,
            *mut *mut c_void,
            *mut *const c_char,
        ) -> c_int;
        type FinalizeFn = unsafe extern "C" fn(*mut c_void) -> c_int;
        type CloseFn = unsafe extern "C" fn(*mut c_void) -> c_int;

        let open = libc::dlsym(handle, b"sqlite3_open\0".as_ptr() as *const c_char);
        let prepare = libc::dlsym(handle, b"sqlite3_prepare_v2\0".as_ptr() as *const c_char);
        let finalize = libc::dlsym(handle, b"sqlite3_finalize\0".as_ptr() as *const c_char);
        let close = libc::dlsym(handle, b"sqlite3_close\0".as_ptr() as *const c_char);

        assert!(!open.is_null());
        assert!(!prepare.is_null());
        assert!(!finalize.is_null());
        assert!(!close.is_null());

        let open: OpenFn = std::mem::transmute(open);
        let prepare: PrepareFn = std::mem::transmute(prepare);
        let finalize: FinalizeFn = std::mem::transmute(finalize);
        let close: CloseFn = std::mem::transmute(close);

        let mut db: *mut c_void = std::ptr::null_mut();
        let rc = open(b":memory:\0".as_ptr() as *const c_char, &mut db);
        assert_eq!(rc, 0);

        let mut stmt: *mut c_void = std::ptr::null_mut();
        let mut tail: *const c_char = std::ptr::null();
        let sql = b"SELECT 1\0";
        let rc2 = prepare(db, sql.as_ptr() as *const c_char, -1, &mut stmt, &mut tail);
        assert_eq!(rc2, 0);

        let _ = finalize(stmt);
        let _ = close(db);
        libc::dlclose(handle);
    }
}
