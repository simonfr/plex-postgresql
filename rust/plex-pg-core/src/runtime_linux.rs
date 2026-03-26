#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

#[allow(unused_imports)]
use crate::c_abi;
use crate::db_interpose_common;
use crate::db_interpose_common::stderr_ptr;
use crate::env_utils;
use crate::exception_what::pg_exception_install_terminate_logger;
#[allow(unused_imports)]
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value};
use crate::runtime_common::{handle_exception_with_tls, log_shim_unloading, shim_init_common};

type SigactionFn =
    unsafe extern "C" fn(c_int, *const libc::sigaction, *mut libc::sigaction) -> c_int;
type CxaThrowFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, Option<unsafe extern "C" fn(*mut c_void)>) -> !;

static mut ORIG_SIGACTION: Option<SigactionFn> = None;
static mut ORIG_CXA_THROW: Option<CxaThrowFn> = None;

static FORCE_IGNORE_SIGCHLD: AtomicI32 = AtomicI32::new(1);
static INTERCEPT_SIGACTION: AtomicI32 = AtomicI32::new(1);
static SIGNAL_LOG_ENABLED_CACHED: AtomicI32 = AtomicI32::new(-1);
static EXCEPTION_CATCHER_ENABLED_CACHED: AtomicI32 = AtomicI32::new(-1);

pub(crate) fn disable_postfork_signal_overrides_fast() {
    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
}

fn signal_log_enabled() -> bool {
    let cached = SIGNAL_LOG_ENABLED_CACHED.load(Ordering::Acquire);
    if cached != -1 {
        return cached != 0;
    }
    let enabled = env_utils::env_truthy(b"PLEX_PG_ENABLE_SIGNAL_LOG\0");
    SIGNAL_LOG_ENABLED_CACHED.store(if enabled { 1 } else { 0 }, Ordering::Release);
    enabled
}

fn exception_catcher_enabled() -> bool {
    let cached = EXCEPTION_CATCHER_ENABLED_CACHED.load(Ordering::Acquire);
    if cached != -1 {
        return cached != 0;
    }
    let enabled = env_utils::env_truthy(b"PLEX_PG_ENABLE_EXCEPTION_CATCHER\0");
    EXCEPTION_CATCHER_ENABLED_CACHED.store(if enabled { 1 } else { 0 }, Ordering::Release);
    enabled
}

unsafe fn resolve_cxa_throw() -> Option<CxaThrowFn> {
    if let Some(f) = ptr::read(ptr::addr_of!(ORIG_CXA_THROW)) {
        return Some(f);
    }
    let sym = libc::dlsym(libc::RTLD_NEXT, b"__cxa_throw\0".as_ptr() as *const c_char);
    if sym.is_null() {
        return None;
    }
    let f: CxaThrowFn = std::mem::transmute::<*mut c_void, CxaThrowFn>(sym);
    ptr::write(ptr::addr_of_mut!(ORIG_CXA_THROW), Some(f));
    Some(f)
}

fn setup_exception_catcher_if_enabled() {
    if !exception_catcher_enabled() {
        return;
    }
    unsafe {
        if resolve_cxa_throw().is_some() {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Exception catcher enabled (__cxa_throw interposed)\n\0".as_ptr()
                    as *const c_char,
            );
            pg_exception_install_terminate_logger();
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Exception terminate logger requested (see [EXC_TERMINATE])\n\0"
                    .as_ptr() as *const c_char,
            );
        } else {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] WARNING: failed to resolve __cxa_throw\n\0".as_ptr() as *const c_char,
            );
        }
        let _ = libc::fflush(stderr_ptr());
    }
}

#[no_mangle]
/// # Safety
/// This is an ABI-level interposition hook for C++ exceptions.
/// Callers must follow the platform C++ ABI for `__cxa_throw`.
pub unsafe extern "C" fn __cxa_throw(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
    dest: Option<unsafe extern "C" fn(*mut c_void)>,
) -> ! {
    let orig = match resolve_cxa_throw() {
        Some(f) => f,
        None => libc::abort(),
    };

    if !exception_catcher_enabled() {
        orig(thrown_exception, tinfo, dest);
    }

    let (handled, _should_call_original) = handle_exception_with_tls(thrown_exception, tinfo);

    if handled == 0 {
        orig(thrown_exception, tinfo, dest);
    }

    orig(thrown_exception, tinfo, dest);
}

#[cfg(target_env = "musl")]
unsafe fn install_signal_handler(signum: c_int) {
    let handler: extern "C" fn(c_int) = db_interpose_common::common_signal_handler;
    libc::signal(signum, handler as libc::sighandler_t);
}

#[cfg(not(target_env = "musl"))]
unsafe fn install_signal_handler(signum: c_int) {
    let handler: extern "C" fn(c_int) = db_interpose_common::common_signal_handler;
    libc::signal(
        signum,
        handler as libc::sighandler_t,
    );
}

#[no_mangle]
/// # Safety
/// This is an ABI-level interposition hook for `sigaction`. The caller must
/// provide valid pointers (or NULL where allowed by the libc API).
pub unsafe extern "C" fn sigaction(
    signum: c_int,
    act: *const libc::sigaction,
    oldact: *mut libc::sigaction,
) -> c_int {
    if ptr::read(ptr::addr_of!(ORIG_SIGACTION)).is_none() {
        let sym = libc::dlsym(libc::RTLD_NEXT, b"sigaction\0".as_ptr() as *const c_char);
        if !sym.is_null() {
            ptr::write(ptr::addr_of_mut!(ORIG_SIGACTION), Some(std::mem::transmute::<*mut c_void, SigactionFn>(sym)));
        } else {
            return -1;
        }
    }

    let Some(orig) = ptr::read(ptr::addr_of!(ORIG_SIGACTION)) else {
        return -1;
    };

    if INTERCEPT_SIGACTION.load(Ordering::Relaxed) == 0 {
        return orig(signum, act, oldact);
    }

    if FORCE_IGNORE_SIGCHLD.load(Ordering::Relaxed) != 0
        && signum == libc::SIGCHLD
        && !act.is_null()
    {
        if !oldact.is_null() {
            orig(libc::SIGCHLD, ptr::null(), oldact);
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_NOCLDSTOP;
        return orig(libc::SIGCHLD, &sa, ptr::null_mut());
    }

    if signal_log_enabled()
        && !act.is_null()
        && (signum == libc::SIGSEGV
            || signum == libc::SIGABRT
            || signum == libc::SIGFPE
            || signum == libc::SIGILL
            || {
                #[cfg(target_os = "linux")]
                {
                    signum == libc::SIGBUS
                }
                #[cfg(not(target_os = "linux"))]
                {
                    false
                }
            })
    {
        if !oldact.is_null() {
            orig(signum, ptr::null(), oldact);
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction =
            db_interpose_common::common_signal_handler as extern "C" fn(c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        return orig(signum, &sa, ptr::null_mut());
    }

    orig(signum, act, oldact)
}

static mut REAL_SQLITE_HANDLE: *mut c_void = ptr::null_mut();

unsafe fn load_original_functions() {
    let sqlite_paths: [&[u8]; 3] = [
        b"/usr/local/lib/plex-postgresql/libsqlite3_real.so\0",
        b"/usr/lib/plexmediaserver/lib/libsqlite3.so.original\0",
        b"/usr/lib/plexmediaserver/lib/libsqlite3.so\0",
    ];

    let mut handle: *mut c_void = ptr::null_mut();
    for path in sqlite_paths.iter() {
        handle = libc::dlopen(
            path.as_ptr() as *const c_char,
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if !handle.is_null() {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Loaded real SQLite from %s\n\0".as_ptr() as *const c_char,
                path.as_ptr() as *const c_char,
            );
            ptr::write(ptr::addr_of_mut!(REAL_SQLITE_HANDLE), handle);
            break;
        }
    }

    if handle.is_null() {
        let _ = libc::fprintf(
            stderr_ptr(),
            b"[SHIM_INIT] Loading original SQLite functions via RTLD_NEXT...\n\0".as_ptr()
                as *const c_char,
        );
        handle = libc::RTLD_NEXT;
    }

    db_interpose_common::common_load_sqlite_symbols(handle);
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[SHIM_INIT] Original SQLite functions loaded\n\0".as_ptr() as *const c_char,
    );
}

#[no_mangle]
pub extern "C" fn ensure_real_sqlite_loaded() {
    unsafe {
        if ptr::read(ptr::addr_of!(db_interpose_common::shim_sqlite3_prepare_v2)).is_some() {
            return;
        }
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_prepare_v2),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_prepare_v2)),
        );
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_errmsg),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_errmsg)),
        );
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_errcode),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_errcode)),
        );
    }
}

unsafe extern "C" fn shim_init() {
    shim_init_common(
        "Linux",
        || {
            // Process name filtering: skip non-server/scanner processes.
            if let Ok(cmdline) = std::fs::read("/proc/self/cmdline") {
                let mut base = cmdline.as_slice();
                if let Some(pos) = cmdline.iter().rposition(|&b| b == b'/') {
                    base = &cmdline[pos + 1..];
                }
                if let Some(pos) = base.iter().position(|&b| b == 0) {
                    base = &base[..pos];
                }
                let base_str = std::str::from_utf8(base).unwrap_or_default();
                if !base_str.contains("Plex Media Server")
                    && !base_str.contains("Plex Media Scanner")
                {
                    crate::pms_child_env::maybe_reexec_current_process_without_shim(
                        base_str, &cmdline,
                    );
                    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
                    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
                    ptr::write(ptr::addr_of_mut!(db_interpose_common::shim_passthrough_only), 1);
                    load_original_functions();
                    ptr::write(ptr::addr_of_mut!(db_interpose_common::shim_initialized), 1);
                    let base_c = CString::new(base_str).unwrap_or_default();
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] Not Plex Server/Scanner ('%s'), skipping entirely (PID %d)\n\0"
                            .as_ptr() as *const c_char,
                        base_c.as_ptr(),
                        libc::getpid(),
                    );
                    let _ = libc::fflush(stderr_ptr());
                    return false;
                }

                if env_utils::env_truthy(b"PLEX_PG_DISABLE_SIGCHLD_IGNORE\0") {
                    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
                }
                if env_utils::env_truthy(b"PLEX_PG_FORCE_SIGCHLD_IGNORE\0") {
                    FORCE_IGNORE_SIGCHLD.store(1, Ordering::Relaxed);
                }
                if env_utils::env_truthy(b"PLEX_PG_DISABLE_SIGACTION_INTERCEPT\0") {
                    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
                }
            }

            db_interpose_common::common_check_fork();

            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Fork safety: using PID-based detection (no pthread_atfork)\n\0"
                    .as_ptr() as *const c_char,
            );
            let _ = libc::fflush(stderr_ptr());

            load_original_functions();

            if ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_open)).is_none()
                || ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_prepare_v2)).is_none()
            {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] SQLite not found in this process, skipping initialization\n\0"
                        .as_ptr() as *const c_char,
                );
                let _ = libc::fflush(stderr_ptr());
                return false;
            }

            true
        },
        || {},
        || {
            setup_exception_catcher_if_enabled();
            crate::pms_child_env::configure_from_env();
            crate::pms_child_env::scrub_current_process_preload();
            crate::pms_process_compat::configure_from_env();
            crate::pms_net_compat::configure_from_env();

            if env_utils::env_truthy(b"PLEX_PG_ENABLE_SIGNAL_LOG\0") {
                install_signal_handler(libc::SIGSEGV);
                install_signal_handler(libc::SIGABRT);
                install_signal_handler(libc::SIGFPE);
                install_signal_handler(libc::SIGILL);
                #[cfg(target_os = "linux")]
                {
                    install_signal_handler(libc::SIGBUS);
                }
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] Signal logging ENABLED via PLEX_PG_ENABLE_SIGNAL_LOG (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
                let _ = libc::fflush(stderr_ptr());
            }

            if ptr::read(ptr::addr_of!(ORIG_SIGACTION)).is_none() {
                let sym = libc::dlsym(libc::RTLD_NEXT, b"sigaction\0".as_ptr() as *const c_char);
                if !sym.is_null() {
                    ptr::write(ptr::addr_of_mut!(ORIG_SIGACTION), Some(std::mem::transmute::<*mut c_void, SigactionFn>(sym)));
                }
            }

            if FORCE_IGNORE_SIGCHLD.load(Ordering::Relaxed) != 0 {
                if let Some(orig) = ptr::read(ptr::addr_of!(ORIG_SIGACTION)) {
                    let mut sa: libc::sigaction = std::mem::zeroed();
                    sa.sa_sigaction = libc::SIG_IGN;
                    libc::sigemptyset(&mut sa.sa_mask);
                    sa.sa_flags = libc::SA_NOCLDSTOP;
                    orig(libc::SIGCHLD, &sa, ptr::null_mut());
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] SIGCHLD forced to SIG_IGN (PID %d)\n\0".as_ptr()
                            as *const c_char,
                        libc::getpid(),
                    );
                } else {
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] WARNING: could not resolve sigaction; SIGCHLD policy unchanged (PID %d)\n\0"
                            .as_ptr() as *const c_char,
                        libc::getpid(),
                    );
                }
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] SIGCHLD force-ignore disabled via PLEX_PG_DISABLE_SIGCHLD_IGNORE (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
            }

            if INTERCEPT_SIGACTION.load(Ordering::Relaxed) != 0 {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] sigaction interpose ENABLED (PID %d)\n\0".as_ptr()
                        as *const c_char,
                    libc::getpid(),
                );
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] sigaction interpose DISABLED via PLEX_PG_DISABLE_SIGACTION_INTERCEPT (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
            }
            let _ = libc::fflush(stderr_ptr());
        },
        || {
            if !env_utils::env_truthy(b"PLEX_PG_NO_INIT_DELAY\0") {
                let delay_ms = env_utils::env_string("PLEX_PG_INIT_DELAY_MS")
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(200);
                if delay_ms > 0 {
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] Waiting %d ms for symbol resolution (PID %d)...\n\0".as_ptr()
                            as *const c_char,
                        delay_ms,
                        libc::getpid(),
                    );
                    let _ = libc::fflush(stderr_ptr());
                    libc::usleep((delay_ms as u32) * 1000);
                }
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] Init delay DISABLED via PLEX_PG_NO_INIT_DELAY\n\0".as_ptr()
                        as *const c_char,
                );
                let _ = libc::fflush(stderr_ptr());
            }
        },
    );
}

unsafe extern "C" fn shim_cleanup() {
    if ptr::read(ptr::addr_of!(db_interpose_common::shim_initialized)) == 0 {
        return;
    }
    log_shim_unloading("Linux");
    db_interpose_common::common_shim_cleanup();
}

extern "C" fn shim_init_wrapper() {
    unsafe { shim_init() }
}

extern "C" fn shim_cleanup_wrapper() {
    unsafe { shim_cleanup() }
}

#[used]
#[cfg_attr(target_os = "linux", link_section = ".init_array")]
static INIT: extern "C" fn() = shim_init_wrapper;

#[used]
#[cfg_attr(target_os = "linux", link_section = ".fini_array")]
static FINI: extern "C" fn() = shim_cleanup_wrapper;

// ────────────────────────────────────────────────────────────────────────────
// LD_PRELOAD wrappers — only compiled when the `interpose` feature is active.
// This avoids duplicate-symbol errors with rusqlite's bundled sqlite3 in
// test targets AND bin targets.  The shared-library (.so) build enables
// `interpose` explicitly via `--features interpose`.
// ────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "interpose")]
mod ld_preload_wrappers {
use super::*;

macro_rules! wrap_db_ret {
    ($name:ident, $ret:ty, $my:ident) => {
        #[no_mangle]
        pub extern "C" fn $name(db: *mut sqlite3) -> $ret {
            c_abi::$my(db)
        }
    };
}

macro_rules! wrap_stmt_ret {
    ($name:ident, $ret:ty, $my:ident) => {
        #[no_mangle]
        pub extern "C" fn $name(stmt: *mut sqlite3_stmt) -> $ret {
            c_abi::$my(stmt)
        }
    };
}

macro_rules! wrap_stmt_idx {
    ($name:ident, $ret:ty, $my:ident) => {
        #[no_mangle]
        pub extern "C" fn $name(stmt: *mut sqlite3_stmt, idx: c_int) -> $ret {
            c_abi::$my(stmt, idx)
        }
    };
}

macro_rules! wrap_val_ret {
    ($name:ident, $ret:ty, $my:ident) => {
        #[no_mangle]
        pub extern "C" fn $name(val: *mut sqlite3_value) -> $ret {
            c_abi::$my(val)
        }
    };
}

wrap_db_ret!(sqlite3_changes, c_int, my_sqlite3_changes);
wrap_db_ret!(sqlite3_changes64, i64, my_sqlite3_changes64);
wrap_db_ret!(sqlite3_last_insert_rowid, i64, my_sqlite3_last_insert_rowid);
wrap_db_ret!(sqlite3_errmsg, *const c_char, my_sqlite3_errmsg);
wrap_db_ret!(sqlite3_errcode, c_int, my_sqlite3_errcode);
wrap_db_ret!(sqlite3_extended_errcode, c_int, my_sqlite3_extended_errcode);

wrap_stmt_ret!(sqlite3_step, c_int, my_sqlite3_step);
wrap_stmt_ret!(sqlite3_reset, c_int, my_sqlite3_reset);
wrap_stmt_ret!(sqlite3_finalize, c_int, my_sqlite3_finalize);
wrap_stmt_ret!(sqlite3_clear_bindings, c_int, my_sqlite3_clear_bindings);
wrap_stmt_ret!(sqlite3_column_count, c_int, my_sqlite3_column_count);
wrap_stmt_ret!(sqlite3_data_count, c_int, my_sqlite3_data_count);
wrap_stmt_ret!(
    sqlite3_bind_parameter_count,
    c_int,
    my_sqlite3_bind_parameter_count
);
wrap_stmt_ret!(sqlite3_stmt_readonly, c_int, my_sqlite3_stmt_readonly);
wrap_stmt_ret!(sqlite3_stmt_busy, c_int, my_sqlite3_stmt_busy);
wrap_stmt_ret!(sqlite3_db_handle, *mut sqlite3, my_sqlite3_db_handle);
wrap_stmt_ret!(sqlite3_expanded_sql, *mut c_char, my_sqlite3_expanded_sql);
wrap_stmt_ret!(sqlite3_sql, *const c_char, my_sqlite3_sql);

wrap_stmt_idx!(sqlite3_column_type, c_int, my_sqlite3_column_type);
wrap_stmt_idx!(sqlite3_column_int, c_int, my_sqlite3_column_int);
wrap_stmt_idx!(sqlite3_column_int64, i64, my_sqlite3_column_int64);
wrap_stmt_idx!(sqlite3_column_double, f64, my_sqlite3_column_double);
wrap_stmt_idx!(sqlite3_column_text, *const u8, my_sqlite3_column_text);
wrap_stmt_idx!(sqlite3_column_blob, *const c_void, my_sqlite3_column_blob);
wrap_stmt_idx!(sqlite3_column_bytes, c_int, my_sqlite3_column_bytes);
wrap_stmt_idx!(sqlite3_column_name, *const c_char, my_sqlite3_column_name);
wrap_stmt_idx!(
    sqlite3_column_value,
    *mut sqlite3_value,
    my_sqlite3_column_value
);
wrap_stmt_idx!(
    sqlite3_bind_parameter_name,
    *const c_char,
    my_sqlite3_bind_parameter_name
);

wrap_val_ret!(sqlite3_value_type, c_int, my_sqlite3_value_type);
wrap_val_ret!(sqlite3_value_text, *const u8, my_sqlite3_value_text);
wrap_val_ret!(sqlite3_value_int, c_int, my_sqlite3_value_int);
wrap_val_ret!(sqlite3_value_int64, i64, my_sqlite3_value_int64);
wrap_val_ret!(sqlite3_value_double, f64, my_sqlite3_value_double);
wrap_val_ret!(sqlite3_value_bytes, c_int, my_sqlite3_value_bytes);
wrap_val_ret!(sqlite3_value_blob, *const c_void, my_sqlite3_value_blob);

#[no_mangle]
pub extern "C" fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int {
    c_abi::my_sqlite3_open(filename, db)
}

#[no_mangle]
pub extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    db: *mut *mut sqlite3,
    flags: c_int,
    vfs: *const c_char,
) -> c_int {
    c_abi::my_sqlite3_open_v2(filename, db, flags, vfs)
}

#[no_mangle]
pub extern "C" fn sqlite3_close(db: *mut sqlite3) -> c_int {
    c_abi::my_sqlite3_close(db)
}

#[no_mangle]
pub extern "C" fn sqlite3_close_v2(db: *mut sqlite3) -> c_int {
    c_abi::my_sqlite3_close_v2(db)
}

#[no_mangle]
pub extern "C" fn sqlite3_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    cb: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
    >,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    c_abi::my_sqlite3_exec(db, sql, cb, arg, errmsg)
}

#[no_mangle]
pub extern "C" fn sqlite3_get_table(
    db: *mut sqlite3,
    sql: *const c_char,
    results: *mut *mut *mut c_char,
    nrow: *mut c_int,
    ncol: *mut c_int,
    errmsg: *mut *mut c_char,
) -> c_int {
    c_abi::my_sqlite3_get_table(db, sql, results, nrow, ncol, errmsg)
}

#[no_mangle]
pub extern "C" fn sqlite3_prepare(
    db: *mut sqlite3,
    sql: *const c_char,
    n: c_int,
    stmt: *mut *mut sqlite3_stmt,
    tail: *mut *const c_char,
) -> c_int {
    c_abi::my_sqlite3_prepare(db, sql, n, stmt, tail)
}

#[no_mangle]
pub extern "C" fn sqlite3_prepare_v2(
    db: *mut sqlite3,
    sql: *const c_char,
    n: c_int,
    stmt: *mut *mut sqlite3_stmt,
    tail: *mut *const c_char,
) -> c_int {
    c_abi::my_sqlite3_prepare_v2(db, sql, n, stmt, tail)
}

#[no_mangle]
pub extern "C" fn sqlite3_prepare_v3(
    db: *mut sqlite3,
    sql: *const c_char,
    n: c_int,
    flags: c_int,
    stmt: *mut *mut sqlite3_stmt,
    tail: *mut *const c_char,
) -> c_int {
    c_abi::my_sqlite3_prepare_v3(db, sql, n, flags as u32, stmt, tail)
}

#[no_mangle]
pub extern "C" fn sqlite3_prepare16_v2(
    db: *mut sqlite3,
    sql: *const c_void,
    n: c_int,
    stmt: *mut *mut sqlite3_stmt,
    tail: *mut *const c_void,
) -> c_int {
    c_abi::my_sqlite3_prepare16_v2(db, sql, n, stmt, tail)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_int(stmt: *mut sqlite3_stmt, idx: c_int, val: c_int) -> c_int {
    c_abi::my_sqlite3_bind_int(stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, idx: c_int, val: i64) -> c_int {
    c_abi::my_sqlite3_bind_int64(stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_double(stmt: *mut sqlite3_stmt, idx: c_int, val: f64) -> c_int {
    c_abi::my_sqlite3_bind_double(stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    c_abi::my_sqlite3_bind_null(stmt, idx)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_text(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n: c_int,
    dtor: *mut c_void,
) -> c_int {
    c_abi::my_sqlite3_bind_text(stmt, idx, val, n, dtor)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_text64(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    n: u64,
    dtor: *mut c_void,
    enc: u8,
) -> c_int {
    c_abi::my_sqlite3_bind_text64(stmt, idx, val, n, dtor, enc)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_blob(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n: c_int,
    dtor: *mut c_void,
) -> c_int {
    c_abi::my_sqlite3_bind_blob(stmt, idx, val, n, dtor)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_blob64(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    n: u64,
    dtor: *mut c_void,
) -> c_int {
    c_abi::my_sqlite3_bind_blob64(stmt, idx, val, n, dtor)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_value(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const sqlite3_value,
) -> c_int {
    c_abi::my_sqlite3_bind_value(stmt, idx, val)
}

#[no_mangle]
pub extern "C" fn sqlite3_bind_parameter_index(
    stmt: *mut sqlite3_stmt,
    name: *const c_char,
) -> c_int {
    c_abi::my_sqlite3_bind_parameter_index(stmt, name)
}

#[no_mangle]
pub extern "C" fn sqlite3_stmt_status(stmt: *mut sqlite3_stmt, op: c_int, reset: c_int) -> c_int {
    c_abi::my_sqlite3_stmt_status(stmt, op, reset)
}

#[no_mangle]
pub extern "C" fn sqlite3_free(ptr: *mut c_void) {
    c_abi::my_sqlite3_free(ptr);
}

#[no_mangle]
pub extern "C" fn sqlite3_malloc(n: c_int) -> *mut c_void {
    c_abi::my_sqlite3_malloc(n)
}

#[no_mangle]
pub extern "C" fn sqlite3_create_collation(
    db: *mut sqlite3,
    name: *const c_char,
    enc: c_int,
    arg: *mut c_void,
    cmp: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
    >,
) -> c_int {
    c_abi::my_sqlite3_create_collation(db, name, enc, arg, cmp)
}

#[no_mangle]
pub extern "C" fn sqlite3_create_collation_v2(
    db: *mut sqlite3,
    name: *const c_char,
    enc: c_int,
    arg: *mut c_void,
    cmp: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
    >,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    c_abi::my_sqlite3_create_collation_v2(db, name, enc, arg, cmp, destroy)
}

#[no_mangle]
pub extern "C" fn sqlite3_column_decltype(stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char {
    c_abi::my_sqlite3_column_decltype(stmt, idx)
}

} // mod ld_preload_wrappers
