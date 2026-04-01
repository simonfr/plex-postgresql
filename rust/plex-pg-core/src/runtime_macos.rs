#![allow(
    clippy::missing_transmute_annotations,
    clippy::transmutes_expressible_as_ptr_casts
)]

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::c_abi;
use crate::db_interpose_common;
use crate::db_interpose_common::stderr_ptr;
use crate::exception_what::pg_exception_install_terminate_logger;
use crate::fishhook::{self, Rebinding};
use crate::runtime_common::{handle_exception_with_tls, log_shim_unloading, shim_init_common};

type CxaThrowFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, Option<unsafe extern "C" fn(*mut c_void)>) -> !;

static mut ORIG_CXA_THROW: Option<CxaThrowFn> = None;

unsafe extern "C" fn my_cxa_throw(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
    dest: Option<unsafe extern "C" fn(*mut c_void)>,
) -> ! {
    let (handled, _should_call_original) = handle_exception_with_tls(thrown_exception, tinfo);

    if handled == 0 {
        if let Some(orig) = std::ptr::read(std::ptr::addr_of!(ORIG_CXA_THROW)) {
            orig(thrown_exception, tinfo, dest);
        }
        libc::abort();
    }

    if let Some(orig) = std::ptr::read(std::ptr::addr_of!(ORIG_CXA_THROW)) {
        orig(thrown_exception, tinfo, dest);
    }
    libc::abort();
}

unsafe fn opt_slot<T>(slot: *mut Option<T>) -> *mut *mut c_void {
    slot as *mut *mut c_void
}

#[inline]
unsafe fn read_option<T: Copy>(slot: *const Option<T>) -> Option<T> {
    std::ptr::read(slot)
}

fn setup_fishhook_rebindings() {
    unsafe {
        let mut rebindings = [
            // Open/Close
            Rebinding {
                name: b"sqlite3_open\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_open as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_open)),
            },
            Rebinding {
                name: b"sqlite3_open_v2\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_open_v2 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_open_v2)),
            },
            Rebinding {
                name: b"sqlite3_close\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_close as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_close)),
            },
            Rebinding {
                name: b"sqlite3_close_v2\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_close_v2 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_close_v2
                )),
            },
            // Exec
            Rebinding {
                name: b"sqlite3_exec\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_exec as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_exec)),
            },
            Rebinding {
                name: b"sqlite3_get_table\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_get_table as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_get_table
                )),
            },
            // Metadata
            Rebinding {
                name: b"sqlite3_changes\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_changes as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_changes)),
            },
            Rebinding {
                name: b"sqlite3_changes64\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_changes64 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_changes64
                )),
            },
            Rebinding {
                name: b"sqlite3_last_insert_rowid\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_last_insert_rowid as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_last_insert_rowid
                )),
            },
            Rebinding {
                name: b"sqlite3_errmsg\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_errmsg as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_errmsg)),
            },
            Rebinding {
                name: b"sqlite3_errcode\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_errcode as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_errcode)),
            },
            Rebinding {
                name: b"sqlite3_extended_errcode\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_extended_errcode as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_extended_errcode
                )),
            },
            // Prepare
            Rebinding {
                name: b"sqlite3_prepare\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_prepare as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_prepare)),
            },
            Rebinding {
                name: b"sqlite3_prepare_v2\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_prepare_v2 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_prepare_v2
                )),
            },
            Rebinding {
                name: b"sqlite3_prepare_v3\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_prepare_v3 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_prepare_v3
                )),
            },
            Rebinding {
                name: b"sqlite3_prepare16_v2\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_prepare16_v2 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_prepare16_v2
                )),
            },
            // Bind
            Rebinding {
                name: b"sqlite3_bind_int\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_int as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_int
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_int64\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_int64 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_int64
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_double\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_double as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_double
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_text\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_text as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_text
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_text64\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_text64 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_text64
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_blob\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_blob as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_blob
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_blob64\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_blob64 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_blob64
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_value\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_value as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_value
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_null\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_null as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_null
                )),
            },
            // Step/Reset/Finalize
            Rebinding {
                name: b"sqlite3_step\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_step as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_step)),
            },
            Rebinding {
                name: b"sqlite3_reset\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_reset as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_reset)),
            },
            Rebinding {
                name: b"sqlite3_finalize\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_finalize as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_finalize
                )),
            },
            Rebinding {
                name: b"sqlite3_clear_bindings\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_clear_bindings as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_clear_bindings
                )),
            },
            // Column access
            Rebinding {
                name: b"sqlite3_column_count\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_count as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_count
                )),
            },
            Rebinding {
                name: b"sqlite3_column_type\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_type as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_type
                )),
            },
            Rebinding {
                name: b"sqlite3_column_int\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_int as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_int
                )),
            },
            Rebinding {
                name: b"sqlite3_column_int64\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_int64 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_int64
                )),
            },
            Rebinding {
                name: b"sqlite3_column_double\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_double as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_double
                )),
            },
            Rebinding {
                name: b"sqlite3_column_text\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_text as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_text
                )),
            },
            Rebinding {
                name: b"sqlite3_column_blob\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_blob as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_blob
                )),
            },
            Rebinding {
                name: b"sqlite3_column_bytes\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_bytes as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_bytes
                )),
            },
            Rebinding {
                name: b"sqlite3_column_name\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_name as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_name
                )),
            },
            Rebinding {
                name: b"sqlite3_column_decltype\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_decltype as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_decltype
                )),
            },
            Rebinding {
                name: b"sqlite3_column_value\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_column_value as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_column_value
                )),
            },
            Rebinding {
                name: b"sqlite3_data_count\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_data_count as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_data_count
                )),
            },
            // Value access
            Rebinding {
                name: b"sqlite3_value_type\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_value_type as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_value_type
                )),
            },
            Rebinding {
                name: b"sqlite3_value_text\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_value_text as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_value_text
                )),
            },
            Rebinding {
                name: b"sqlite3_value_int\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_value_int as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_value_int
                )),
            },
            Rebinding {
                name: b"sqlite3_value_int64\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_value_int64 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_value_int64
                )),
            },
            Rebinding {
                name: b"sqlite3_value_double\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_value_double as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_value_double
                )),
            },
            Rebinding {
                name: b"sqlite3_value_bytes\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_value_bytes as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_value_bytes
                )),
            },
            Rebinding {
                name: b"sqlite3_value_blob\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_value_blob as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_value_blob
                )),
            },
            // Collation
            Rebinding {
                name: b"sqlite3_create_collation\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_create_collation as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_create_collation
                )),
            },
            Rebinding {
                name: b"sqlite3_create_collation_v2\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_create_collation_v2 as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_create_collation_v2
                )),
            },
            // Memory and statement info
            Rebinding {
                name: b"sqlite3_free\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_free as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_free)),
            },
            Rebinding {
                name: b"sqlite3_malloc\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_malloc as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_malloc)),
            },
            Rebinding {
                name: b"sqlite3_db_handle\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_db_handle as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_db_handle
                )),
            },
            Rebinding {
                name: b"sqlite3_sql\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_sql as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(db_interpose_common::orig_sqlite3_sql)),
            },
            Rebinding {
                name: b"sqlite3_expanded_sql\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_expanded_sql as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_expanded_sql
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_parameter_count\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_parameter_count as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_parameter_count
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_parameter_index\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_parameter_index as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_parameter_index
                )),
            },
            Rebinding {
                name: b"sqlite3_stmt_readonly\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_stmt_readonly as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_stmt_readonly
                )),
            },
            Rebinding {
                name: b"sqlite3_stmt_busy\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_stmt_busy as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_stmt_busy
                )),
            },
            Rebinding {
                name: b"sqlite3_stmt_status\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_stmt_status as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_stmt_status
                )),
            },
            Rebinding {
                name: b"sqlite3_bind_parameter_name\0".as_ptr() as *const c_char,
                replacement: c_abi::my_sqlite3_bind_parameter_name as *const c_void,
                replaced: opt_slot(ptr::addr_of_mut!(
                    db_interpose_common::orig_sqlite3_bind_parameter_name
                )),
            },
        ];

        let result = fishhook::rebind_symbols(&mut rebindings);
        if result == 0 {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] fishhook rebind_symbols succeeded for %d functions\n\0".as_ptr()
                    as *const c_char,
                rebindings.len() as c_int,
            );
        } else {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] ERROR: fishhook rebind_symbols failed with code %d\n\0".as_ptr()
                    as *const c_char,
                result,
            );
        }
    }
}

fn setup_exception_rebinding_if_enabled() {
    unsafe {
        let enable = libc::getenv(b"PLEX_PG_ENABLE_EXCEPTION_CATCHER\0".as_ptr() as *const c_char);
        if enable.is_null() || *enable == b'0' as c_char {
            return;
        }

        let mut rebinding = [Rebinding {
            name: b"__cxa_throw\0".as_ptr() as *const c_char,
            replacement: my_cxa_throw as *const c_void,
            replaced: opt_slot(ptr::addr_of_mut!(ORIG_CXA_THROW)),
        }];

        let rc = fishhook::rebind_symbols(&mut rebinding);
        if rc == 0 {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Exception catcher enabled (__cxa_throw hooked)\n\0".as_ptr()
                    as *const c_char,
            );
            pg_exception_install_terminate_logger();
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Exception terminate logger enabled\n\0".as_ptr() as *const c_char,
            );
        } else {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] WARNING: failed to hook __cxa_throw (rc=%d)\n\0".as_ptr()
                    as *const c_char,
                rc,
            );
        }
        let _ = libc::fflush(stderr_ptr());
    }
}

unsafe fn load_sqlite_fallback() {
    let sqlite_paths: [&[u8]; 3] = [
        b"/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3_orig.dylib\0",
        b"/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3.dylib\0",
        b"/usr/lib/libsqlite3.dylib\0",
    ];

    for path in sqlite_paths.iter() {
        if !std::ptr::read(std::ptr::addr_of!(db_interpose_common::sqlite_handle)).is_null() {
            break;
        }
        let handle = libc::dlopen(
            path.as_ptr() as *const c_char,
            libc::RTLD_LAZY | libc::RTLD_LOCAL,
        );
        if !handle.is_null() {
            std::ptr::write(
                std::ptr::addr_of_mut!(db_interpose_common::sqlite_handle),
                handle,
            );
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Loaded SQLite fallback from: %s\n\0".as_ptr() as *const c_char,
                path.as_ptr() as *const c_char,
            );
        }
    }

    let sqlite_h = std::ptr::read(std::ptr::addr_of!(db_interpose_common::sqlite_handle));
    if !sqlite_h.is_null()
        && (read_option(std::ptr::addr_of!(
            db_interpose_common::shim_sqlite3_prepare_v2
        ))
        .is_none()
            || read_option(std::ptr::addr_of!(
                db_interpose_common::orig_sqlite3_prepare_v2
            ))
            .is_none())
    {
        let _ = libc::fprintf(
            stderr_ptr(),
            b"[SHIM_INIT] Fishhook incomplete, using dlsym fallback\n\0".as_ptr() as *const c_char,
        );
        db_interpose_common::common_load_sqlite_symbols(sqlite_h);
    }
}

#[no_mangle]
pub extern "C" fn ensure_real_sqlite_loaded() {
    unsafe {
        if read_option(std::ptr::addr_of!(
            db_interpose_common::shim_sqlite3_prepare_v2
        ))
        .is_some()
        {
            return;
        }
        let sqlite_h = std::ptr::read(std::ptr::addr_of!(db_interpose_common::sqlite_handle));
        if sqlite_h.is_null() {
            load_sqlite_fallback();
        }
        let sqlite_h = std::ptr::read(std::ptr::addr_of!(db_interpose_common::sqlite_handle));
        if !sqlite_h.is_null() {
            std::ptr::write(
                std::ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_prepare_v2),
                Some(std::mem::transmute(libc::dlsym(
                    sqlite_h,
                    b"sqlite3_prepare_v2\0".as_ptr() as *const c_char,
                ))),
            );
            std::ptr::write(
                std::ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_errmsg),
                Some(std::mem::transmute(libc::dlsym(
                    sqlite_h,
                    b"sqlite3_errmsg\0".as_ptr() as *const c_char,
                ))),
            );
            std::ptr::write(
                std::ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_errcode),
                Some(std::mem::transmute(libc::dlsym(
                    sqlite_h,
                    b"sqlite3_errcode\0".as_ptr() as *const c_char,
                ))),
            );
        }
    }
}

unsafe extern "C" fn shim_init() {
    shim_init_common(
        "macOS",
        || {
            db_interpose_common::common_check_fork();

            if cfg!(debug_assertions) {
                let handler: libc::sighandler_t = std::mem::transmute(
                    db_interpose_common::common_signal_handler as extern "C" fn(c_int),
                );
                libc::signal(libc::SIGSEGV, handler);
                libc::signal(libc::SIGABRT, handler);
                libc::signal(libc::SIGBUS, handler);
                libc::signal(libc::SIGFPE, handler);
                libc::signal(libc::SIGILL, handler);
            }

            libc::pthread_atfork(
                Some(db_interpose_common::common_atfork_prepare),
                Some(db_interpose_common::common_atfork_parent),
                Some(db_interpose_common::common_atfork_child),
            );
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Registered pthread_atfork handlers\n\0".as_ptr() as *const c_char,
            );
            let _ = libc::fflush(stderr_ptr());

            true
        },
        || {
            setup_fishhook_rebindings();
            setup_exception_rebinding_if_enabled();
            load_sqlite_fallback();
        },
        || {},
        || {},
    );
}

unsafe extern "C" fn shim_cleanup() {
    if db_interpose_common::SHIM_INITIALIZED.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return;
    }

    log_shim_unloading("macOS");
    db_interpose_common::common_shim_cleanup();
}

extern "C" fn shim_init_wrapper() {
    unsafe {
        shim_init();
    }
}

extern "C" fn shim_cleanup_wrapper() {
    unsafe {
        shim_cleanup();
    }
}

#[used]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
static INIT: extern "C" fn() = shim_init_wrapper;

#[used]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_term_func")]
static FINI: extern "C" fn() = shim_cleanup_wrapper;
