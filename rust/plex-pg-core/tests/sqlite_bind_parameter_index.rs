use rusqlite::{Connection, Result};
use rusqlite::ffi;
use std::ffi::CString;

fn prepare(conn: &Connection, sql: &str) -> Result<*mut ffi::sqlite3_stmt> {
    let mut stmt: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
    let mut tail: *const std::os::raw::c_char = std::ptr::null();
    let csql = CString::new(sql).unwrap();
    let rc = unsafe { ffi::sqlite3_prepare_v2(conn.handle(), csql.as_ptr(), -1, &mut stmt, &mut tail) };
    assert_eq!(rc, ffi::SQLITE_OK);
    Ok(stmt)
}

#[test]
fn bind_parameter_index_basic() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT * FROM sqlite_master WHERE name = :id")?;
    let name = CString::new(":id").unwrap();
    let idx = unsafe { ffi::sqlite3_bind_parameter_index(stmt, name.as_ptr()) };
    assert_eq!(idx, 1);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_multiple_params() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT :name, :age")?;
    let idx_name = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":name").unwrap().as_ptr()) };
    let idx_age = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":age").unwrap().as_ptr()) };
    assert_eq!(idx_name, 1);
    assert_eq!(idx_age, 2);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_same_name_twice() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT :id, :other, :id")?;
    let idx1 = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":id").unwrap().as_ptr()) };
    let idx2 = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":id").unwrap().as_ptr()) };
    let idx_other = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":other").unwrap().as_ptr()) };
    let count = unsafe { ffi::sqlite3_bind_parameter_count(stmt) };
    assert_eq!(idx1, idx2);
    assert_eq!(idx1, 1);
    assert_eq!(idx_other, 2);
    assert_eq!(count, 2);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_not_found() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT :existing")?;
    let idx = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":nonexistent").unwrap().as_ptr()) };
    assert_eq!(idx, 0);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_mixed_positional() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT ?, :name, ?")?;
    let count = unsafe { ffi::sqlite3_bind_parameter_count(stmt) };
    let idx_name = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":name").unwrap().as_ptr()) };
    assert_eq!(count, 3);
    assert_eq!(idx_name, 2);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_at_syntax() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT @param1, @param2")?;
    let idx1 = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new("@param1").unwrap().as_ptr()) };
    let idx2 = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new("@param2").unwrap().as_ptr()) };
    assert_eq!(idx1, 1);
    assert_eq!(idx2, 2);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_dollar_syntax() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT $user, $pass")?;
    let idx1 = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new("$user").unwrap().as_ptr()) };
    let idx2 = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new("$pass").unwrap().as_ptr()) };
    assert_eq!(idx1, 1);
    assert_eq!(idx2, 2);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_null_stmt_is_zero() {
    let idx = unsafe { ffi::sqlite3_bind_parameter_index(std::ptr::null_mut(), CString::new(":param").unwrap().as_ptr()) };
    assert_eq!(idx, 0);
}

#[test]
fn bind_parameter_index_null_name_is_zero() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT :param")?;
    let idx = unsafe { ffi::sqlite3_bind_parameter_index(stmt, std::ptr::null()) };
    assert_eq!(idx, 0);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_empty_name_is_zero() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT :param")?;
    let idx = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new("").unwrap().as_ptr()) };
    assert_eq!(idx, 0);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_case_sensitive() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let stmt = prepare(&conn, "SELECT :MyParam")?;
    let idx_exact = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":MyParam").unwrap().as_ptr()) };
    let idx_lower = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":myparam").unwrap().as_ptr()) };
    let idx_upper = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":MYPARAM").unwrap().as_ptr()) };
    assert_eq!(idx_exact, 1);
    assert_eq!(idx_lower, 0);
    assert_eq!(idx_upper, 0);
    unsafe { ffi::sqlite3_finalize(stmt) };
    Ok(())
}

#[test]
fn bind_parameter_index_with_bind_executes() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute("CREATE TABLE test(id INTEGER, name TEXT)", [])?;
    let stmt = prepare(&conn, "INSERT INTO test VALUES(:id, :name)")?;

    let idx_id = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":id").unwrap().as_ptr()) };
    let idx_name = unsafe { ffi::sqlite3_bind_parameter_index(stmt, CString::new(":name").unwrap().as_ptr()) };

    unsafe {
        ffi::sqlite3_bind_int(stmt, idx_id, 42);
        ffi::sqlite3_bind_text(stmt, idx_name, CString::new("test_user").unwrap().as_ptr(), -1, ffi::SQLITE_TRANSIENT());
    }

    let rc = unsafe { ffi::sqlite3_step(stmt) };
    unsafe { ffi::sqlite3_finalize(stmt) };
    assert_eq!(rc, ffi::SQLITE_DONE);

    let mut select = conn.prepare("SELECT id, name FROM test")?;
    let row = select.query_row([], |r| Ok((r.get::<_, i32>(0)?, r.get::<_, String>(1)?)))?;
    assert_eq!(row.0, 42);
    assert_eq!(row.1, "test_user");
    Ok(())
}
