use super::*;

#[test]
fn stmt_free_sweeps_extra_param_values_without_crash() {
    unsafe {
        let sql = cs("SELECT 1");
        let stmt = rust_stmt_create(std::ptr::null_mut(), sql.as_ptr(), std::ptr::null_mut());
        assert!(!stmt.is_null());

        (*stmt).param_count = 1;

        let a = libc::malloc(16) as *mut c_char;
        let b = libc::malloc(1024 * 1024) as *mut c_char;
        assert!(!a.is_null());
        assert!(!b.is_null());

        (*stmt).param_values[0] = a;
        (*stmt).param_values[100] = b;

        (*stmt).ref_count.store(0, Ordering::Release);
        rust_stmt_free(stmt);
    }
}

#[test]
fn stmt_unref_cleans_bind_index_mismatch_slots() {
    unsafe {
        let sql = cs("SELECT ?");
        let stmt = rust_stmt_create(std::ptr::null_mut(), sql.as_ptr(), std::ptr::null_mut());
        assert!(!stmt.is_null());

        (*stmt).param_count = 1;

        for i in 1..16 {
            let buf = libc::malloc(256) as *mut c_char;
            assert!(!buf.is_null());
            *buf = b'x' as c_char;
            *buf.add(1) = 0;
            (*stmt).param_values[i] = buf;
        }

        (*stmt).ref_count.store(1, Ordering::Release);
        rust_stmt_unref(stmt);
    }
}
