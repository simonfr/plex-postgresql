use super::*;

extern "C" {
    fn pg_pool_cleanup_after_fork();
    fn pg_logging_reset_after_fork();
    fn pg_config_init();
    fn pg_client_init();
    fn pg_statement_init();
    fn pg_query_cache_init();
    fn sql_translator_init();
    fn pg_statement_cleanup();
    fn pg_client_cleanup();
    fn sql_translator_cleanup();
    fn pg_logging_cleanup();
}

#[no_mangle]
pub extern "C" fn rust_common_atfork_prepare() {}

#[no_mangle]
pub extern "C" fn rust_common_atfork_parent() {}

#[no_mangle]
pub extern "C" fn rust_common_atfork_child() {
    unsafe {
        libc::fprintf(
            stderr_ptr(),
            b"[FORK_CHILD] Cleaning up inherited connection pool (child PID %d)\n\0".as_ptr()
                as *const c_char,
            libc::getpid(),
        );
        libc::fflush(stderr_ptr());

        worker_thread = 0 as libc::pthread_t;
        worker_running = 0;
        worker_request = EMPTY_WORKER_REQUEST;
        CRASH_LAST_COLUMN_LEN.store(0, Ordering::SeqCst);
        GLOBAL_VALUE_TYPE_CALLS.store(0, Ordering::Relaxed);
        GLOBAL_COLUMN_TYPE_CALLS.store(0, Ordering::Relaxed);

        rust_reset_exception_tracking();
        rust_reset_symbol_verification();

        pg_pool_cleanup_after_fork();
        pg_logging_reset_after_fork();

        libc::fprintf(
            stderr_ptr(),
            b"[FORK_CHILD] Pool and logging reset, child will reinitialize\n\0".as_ptr()
                as *const c_char,
        );
        libc::fflush(stderr_ptr());
    }
}

#[no_mangle]
pub extern "C" fn rust_common_check_fork() -> c_int {
    let current_pid = unsafe { libc::getpid() };
    unsafe {
        if shim_init_pid != 0 && shim_init_pid != current_pid {
            libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Detected fork (parent PID %d, our PID %d) - resetting state\n\0"
                    .as_ptr() as *const c_char,
                shim_init_pid,
                current_pid,
            );
            libc::fflush(stderr_ptr());

            SHIM_INITIALIZED.store(0, Ordering::Release);
            CRASH_LAST_COLUMN_LEN.store(0, Ordering::SeqCst);
            GLOBAL_VALUE_TYPE_CALLS.store(0, Ordering::Relaxed);
            GLOBAL_COLUMN_TYPE_CALLS.store(0, Ordering::Relaxed);
            rust_reset_exception_tracking();

            shim_init_pid = current_pid;
            return 1;
        }

        shim_init_pid = current_pid;
    }
    0
}

#[no_mangle]
pub extern "C" fn rust_common_shim_init_modules() {
    unsafe {
        pg_config_init();
        pg_client_init();
        pg_statement_init();
        pg_query_cache_init();
        sql_translator_init();
    }
    rust_worker_init();
}

#[no_mangle]
pub extern "C" fn rust_common_shim_cleanup() {
    rust_worker_cleanup();
    unsafe {
        pg_statement_cleanup();
        pg_client_cleanup();
        sql_translator_cleanup();
        pg_logging_cleanup();
    }
}

#[no_mangle]
pub extern "C" fn common_atfork_prepare() {
    rust_common_atfork_prepare();
}

#[no_mangle]
pub extern "C" fn common_atfork_parent() {
    rust_common_atfork_parent();
}

#[no_mangle]
pub extern "C" fn common_atfork_child() {
    rust_common_atfork_child();
}

#[no_mangle]
pub extern "C" fn common_check_fork() -> c_int {
    rust_common_check_fork()
}

#[no_mangle]
pub extern "C" fn common_shim_init_modules() {
    rust_common_shim_init_modules();
}

#[no_mangle]
pub extern "C" fn common_shim_cleanup() {
    rust_common_shim_cleanup();
}
