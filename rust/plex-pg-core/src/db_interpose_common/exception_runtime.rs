use super::*;
use crate::db_interpose_conn_utils::cstr_to_lossy_or;
use crate::log_info_lazy;
use std::ffi::CStr;

extern "C" {
    fn pg_exception_extract_what(
        thrown_exception: *mut c_void,
        tinfo: *mut c_void,
        out_buf: *mut c_char,
        out_buf_len: libc::size_t,
    ) -> c_int;

    fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int);
}

const MAX_LOGGED_PER_TYPE: c_int = 3;
const MAX_LOGGED_TOTAL: c_int = 50;

static EXC_LOG_META_ENV: &[u8] = b"PLEX_PG_EXCEPTION_LOG_META\0";
static EXC_DUMP_OBJECT_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_OBJECT\0";
static EXC_DUMP_BYTES_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_BYTES\0";
static EXC_DUMP_POINTERS_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_POINTERS\0";
static EXC_DUMP_TINFO_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_TINFO\0";
static EXC_DUMP_POINTER_MAX_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_POINTERS_MAX\0";
static EXC_DUMP_POINTER_BYTES_ENV: &[u8] = b"PLEX_PG_EXCEPTION_DUMP_POINTER_BYTES\0";
static EXC_DUMP_SCAN_STRINGS_ENV: &[u8] = b"PLEX_PG_EXCEPTION_SCAN_STRINGS\0";
static EXC_DUMP_SCAN_STRINGS_BYTES_ENV: &[u8] = b"PLEX_PG_EXCEPTION_SCAN_STRINGS_BYTES\0";

pub fn rust_common_signal_handler(sig: c_int) {
    let (sig_name, sig_desc) = match sig {
        libc::SIGSEGV => (
            b"SIGSEGV\0".as_ptr() as *const c_char,
            b"Segmentation fault\0".as_ptr() as *const c_char,
        ),
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        libc::SIGBUS => (
            b"SIGBUS\0".as_ptr() as *const c_char,
            b"Bus error\0".as_ptr() as *const c_char,
        ),
        libc::SIGFPE => (
            b"SIGFPE\0".as_ptr() as *const c_char,
            b"Floating point exception\0".as_ptr() as *const c_char,
        ),
        libc::SIGILL => (
            b"SIGILL\0".as_ptr() as *const c_char,
            b"Illegal instruction\0".as_ptr() as *const c_char,
        ),
        libc::SIGABRT => (
            b"SIGABRT\0".as_ptr() as *const c_char,
            b"Abort\0".as_ptr() as *const c_char,
        ),
        _ => (
            b"UNKNOWN\0".as_ptr() as *const c_char,
            b"Unknown signal\0".as_ptr() as *const c_char,
        ),
    };

    unsafe {
        let fd = libc::STDERR_FILENO;
        let _ = libc::write(fd, b"\n[SHIM_FATAL] ".as_ptr() as *const c_void, 14);
        let name_cstr = CStr::from_ptr(sig_name);
        let _ = libc::write(
            fd,
            name_cstr.as_ptr() as *const c_void,
            name_cstr.to_bytes().len(),
        );
        let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);

        // --- seqlock-guarded read of CRASH_LAST_PHASE ---
        let p_seq1 = CRASH_LAST_PHASE_SEQ.load(Ordering::Acquire);
        if p_seq1 & 1 == 0 {
            let plen = CRASH_LAST_PHASE_LEN.load(Ordering::SeqCst);
            if plen > 0 && (plen as usize) < CRASH_PHASE_MAX_LEN {
                let p_seq2 = CRASH_LAST_PHASE_SEQ.load(Ordering::Acquire);
                if p_seq1 == p_seq2 {
                    let _ = libc::write(fd, b"Last Phase: ".as_ptr() as *const c_void, 12);
                    let _ = libc::write(
                        fd,
                        ptr::addr_of!(CRASH_LAST_PHASE) as *const c_void,
                        plen as usize,
                    );
                    let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
                }
            }
        }

        // --- seqlock-guarded read of CRASH_LAST_QUERY ---
        let q_seq1 = CRASH_LAST_QUERY_SEQ.load(Ordering::Acquire);
        if q_seq1 & 1 == 0 {
            let qlen = CRASH_LAST_QUERY_LEN.load(Ordering::SeqCst);
            if qlen > 0 && (qlen as usize) < CRASH_QUERY_MAX_LEN {
                let q_seq2 = CRASH_LAST_QUERY_SEQ.load(Ordering::Acquire);
                if q_seq1 == q_seq2 {
                    let _ = libc::write(fd, b"Last Query: ".as_ptr() as *const c_void, 12);
                    let _ = libc::write(
                        fd,
                        ptr::addr_of!(CRASH_LAST_QUERY) as *const c_void,
                        qlen as usize,
                    );
                    let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
                }
            }
        }
    }

    unsafe {
        libc::fprintf(stderr_ptr(), b"\n\0".as_ptr() as *const c_char);
        write_box_line(BOX_TL, BOX_TR);
        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 FATAL SIGNAL: %-64s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
            sig_name,
        );
        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 Description:  %-64s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
            sig_desc,
        );
        write_box_line(BOX_ML, BOX_MR);

        // --- seqlock-guarded read of CRASH_LAST_QUERY for box output ---
        let q_box_seq1 = CRASH_LAST_QUERY_SEQ.load(Ordering::Acquire);
        if q_box_seq1 & 1 == 0 {
            let qlen = CRASH_LAST_QUERY_LEN.load(Ordering::SeqCst);
            if qlen > 0 && (qlen as usize) < CRASH_QUERY_MAX_LEN {
                let q_box_seq2 = CRASH_LAST_QUERY_SEQ.load(Ordering::Acquire);
                if q_box_seq1 == q_box_seq2 {
                    let mut q: [c_char; 65] = [0; 65];
                    libc::snprintf(
                        q.as_mut_ptr(),
                        q.len(),
                        b"%.64s\0".as_ptr() as *const c_char,
                        ptr::addr_of!(CRASH_LAST_QUERY) as *const c_char,
                    );
                    libc::fprintf(
                        stderr_ptr(),
                        b"\xE2\x95\x91 Last Query:  %-65s \xE2\x95\x91\n\0".as_ptr()
                            as *const c_char,
                        q.as_ptr(),
                    );
                }
            }
        }

        // --- seqlock-guarded read of CRASH_LAST_COLUMN for box output ---
        let c_box_seq1 = CRASH_LAST_COLUMN_SEQ.load(Ordering::Acquire);
        if c_box_seq1 & 1 == 0 {
            let clen = CRASH_LAST_COLUMN_LEN.load(Ordering::SeqCst);
            if clen > 0 && (clen as usize) < CRASH_COLUMN_MAX_LEN {
                let c_box_seq2 = CRASH_LAST_COLUMN_SEQ.load(Ordering::Acquire);
                if c_box_seq1 == c_box_seq2 {
                    libc::fprintf(
                        stderr_ptr(),
                        b"\xE2\x95\x91 Last Column: %-65s \xE2\x95\x91\n\0".as_ptr()
                            as *const c_char,
                        ptr::addr_of!(CRASH_LAST_COLUMN) as *const c_char,
                    );
                }
            }
        }

        write_box_line(BOX_BL, BOX_BR);
        platform_print_backtrace(sig_name, 1);
    }

    log_error(&format!(
        "FATAL SIGNAL: {} ({})",
        cstr_to_lossy_or(sig_name, "UNKNOWN"),
        cstr_to_lossy_or(sig_desc, "Unknown signal")
    ));

    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

pub fn rust_print_exception_info(
    type_name: *const c_char,
    count: c_int,
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
) -> *mut c_char {
    unsafe {
        let demangle_opt = CXA_DEMANGLE_FN.get_or_init(|| {
            let sym = libc::dlsym(
                libc::RTLD_DEFAULT,
                b"__cxa_demangle\0".as_ptr() as *const c_char,
            );
            if !sym.is_null() {
                Some(std::mem::transmute::<*mut libc::c_void, CxaDemangleFn>(sym))
            } else {
                None
            }
        });

        let mut demangled: *mut c_char = ptr::null_mut();
        if let Some(demangle) = demangle_opt {
            if !type_name.is_null() {
                let mut status: c_int = 0;
                demangled = demangle(type_name, ptr::null_mut(), ptr::null_mut(), &mut status);
            }
        }
        let readable_name = if !demangled.is_null() {
            demangled
        } else {
            type_name
        };

        let ctx_value_calls = GLOBAL_VALUE_TYPE_CALLS.load(Ordering::Relaxed);
        let ctx_column_calls = GLOBAL_COLUMN_TYPE_CALLS.load(Ordering::Relaxed);
        let tls_column_type_calls = *tls_column_type_calls_ptr();
        let tls_value_type_calls = *tls_value_type_calls_ptr();
        let tls_last_query = *tls_last_query_ptr();
        let is_shim_related =
            ctx_value_calls > 0 || ctx_column_calls > 0 || !tls_last_query.is_null();
        let tls_is_shim_related =
            tls_column_type_calls > 0 || tls_value_type_calls > 0 || !tls_last_query.is_null();
        // Read CRASH_LAST_COLUMN into a local buffer (exception handler, not
        // signal-safe context, so a simple copy is fine).
        let mut exc_col_buf: [c_char; CRASH_COLUMN_MAX_LEN] = [0; CRASH_COLUMN_MAX_LEN];
        let exc_col_len = CRASH_LAST_COLUMN_LEN.load(Ordering::SeqCst);
        let has_crash_column = exc_col_len > 0 && (exc_col_len as usize) < CRASH_COLUMN_MAX_LEN;
        if has_crash_column {
            ptr::copy_nonoverlapping(
                ptr::addr_of!(CRASH_LAST_COLUMN) as *const c_char,
                exc_col_buf.as_mut_ptr(),
                exc_col_len as usize + 1,
            );
        }

        let tid = libc::pthread_self();

        libc::fprintf(stderr_ptr(), b"\n\0".as_ptr() as *const c_char);
        write_box_line(BOX_TL, BOX_TR);
        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 C++ EXCEPTION #%-4d                                                          \xE2\x95\x91\n\0"
                .as_ptr() as *const c_char,
            count,
        );
        write_box_line(BOX_ML, BOX_MR);

        let mut type_display: [c_char; 73] = [0; 73];
        if !readable_name.is_null() {
            libc::snprintf(
                type_display.as_mut_ptr(),
                type_display.len(),
                b"%.72s\0".as_ptr() as *const c_char,
                readable_name,
            );
        }
        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 Type: %-72s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
            type_display.as_ptr(),
        );

        let mut what_buf: [c_char; 193] = [0; 193];
        let has_what = pg_exception_extract_what(
            thrown_exception,
            tinfo,
            what_buf.as_mut_ptr(),
            what_buf.len(),
        );
        if has_what != 0 {
            let mut what_display: [c_char; 73] = [0; 73];
            libc::snprintf(
                what_display.as_mut_ptr(),
                what_display.len(),
                b"%.72s\0".as_ptr() as *const c_char,
                what_buf.as_ptr(),
            );
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 What: %-72s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                what_display.as_ptr(),
            );
        } else {
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 What: %-72s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                b"(unavailable at throw site)\0".as_ptr() as *const c_char,
            );
        }

        libc::fprintf(
            stderr_ptr(),
            b"\xE2\x95\x91 PID: %-6d  Thread: 0x%-54lx \xE2\x95\x91\n\0".as_ptr() as *const c_char,
            libc::getpid(),
            tid as libc::c_ulong,
        );

        write_box_line(BOX_ML, BOX_MR);

        if is_shim_related {
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 SHIM STATE:                                                                  \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
            );
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91   Global: col_type=%-5ld val_type=%-5ld                                      \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
                ctx_column_calls,
                ctx_value_calls,
            );
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91   Thread: col_type=%-5ld val_type=%-5ld (this_thread_used_shim=%s)           \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
                tls_column_type_calls,
                tls_value_type_calls,
                if tls_is_shim_related {
                    b"YES\0".as_ptr() as *const c_char
                } else {
                    b"NO \0".as_ptr() as *const c_char
                },
            );
            if !tls_is_shim_related {
                libc::fprintf(
                    stderr_ptr(),
                    b"\xE2\x95\x91   NOTE: This thread has NOT made any SQLite calls through shim!             \xE2\x95\x91\n\0"
                        .as_ptr() as *const c_char,
                );
            }
            if !tls_last_query.is_null() && *tls_last_query != 0 {
                let mut query_snippet: [c_char; 55] = [0; 55];
                libc::snprintf(
                    query_snippet.as_mut_ptr(),
                    query_snippet.len(),
                    b"%.54s\0".as_ptr() as *const c_char,
                    tls_last_query,
                );
                libc::fprintf(
                    stderr_ptr(),
                    b"\xE2\x95\x91   Last Query (this thread): %-50s \xE2\x95\x91\n\0".as_ptr()
                        as *const c_char,
                    query_snippet.as_ptr(),
                );
            }
            if has_crash_column {
                libc::fprintf(
                    stderr_ptr(),
                    b"\xE2\x95\x91   Last Column: %-63s \xE2\x95\x91\n\0".as_ptr() as *const c_char,
                    exc_col_buf.as_ptr(),
                );
            }
        } else {
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 NOT SHIM-RELATED: No SQLite calls have been made through the shim            \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
            );
        }

        log_error(&format!(
            "EXCEPTION #{} [{}]: what='{}' shim={} tls_shim={} col={} val={}",
            count,
            cstr_to_lossy_or(readable_name, ""),
            if has_what != 0 {
                cstr_to_lossy_or(what_buf.as_ptr(), "")
            } else {
                "".into()
            },
            if is_shim_related { "YES" } else { "NO" },
            if tls_is_shim_related { "YES" } else { "NO" },
            ctx_column_calls,
            ctx_value_calls
        ));

        demangled
    }
}

pub fn rust_common_handle_exception(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
    in_handler_flag: *mut c_int,
    should_call_original: *mut c_int,
) -> c_int {
    if in_handler_flag.is_null() || should_call_original.is_null() {
        return 0;
    }

    unsafe {
        *should_call_original = 1;
        if *in_handler_flag != 0 {
            return 0;
        }
        *in_handler_flag = 1;
    }

    let total_count = total_exception_count.fetch_add(1, Ordering::SeqCst) + 1;

    if thrown_exception.is_null() || tinfo.is_null() {
        unsafe {
            *in_handler_flag = 0;
        }
        return 0;
    }

    let type_name = rust_get_type_name(tinfo);
    let tracker = unsafe { get_exception_tracker_impl(type_name) };

    let should_log_meta = env_utils::env_truthy(EXC_LOG_META_ENV);
    let should_dump_object = env_utils::env_truthy(EXC_DUMP_OBJECT_ENV);

    if should_log_meta {
        let type_addr = tinfo as usize;
        let throw_addr = thrown_exception as usize;
        let pid = unsafe { libc::getpid() };
        let tid = unsafe { libc::pthread_self() };
        log_info_lazy!(
            "EXC_META: pid={} tid=0x{:x} thrown=0x{:x} tinfo=0x{:x} total={}",
            pid,
            tid as usize,
            throw_addr,
            type_addr,
            total_count
        );
        if !type_name.is_null() {
            log_info_lazy!(
                "EXC_META: type_name_raw={}",
                cstr_to_lossy_or(type_name, "")
            );
        }
    }
    if should_dump_object {
        let bytes = env_usize(EXC_DUMP_BYTES_ENV).unwrap_or(256);
        let pointers = log_exception_object_dump(thrown_exception, bytes);
        let dump_pointers = env_utils::env_truthy(EXC_DUMP_POINTERS_ENV);
        if dump_pointers {
            let max_ptrs = env_usize(EXC_DUMP_POINTER_MAX_ENV).unwrap_or(6);
            let ptr_bytes = env_usize(EXC_DUMP_POINTER_BYTES_ENV).unwrap_or(512);
            for (idx, ptr) in pointers.into_iter().enumerate() {
                if idx >= max_ptrs {
                    log_info("EXC_META_PTR_DUMP: truncated");
                    break;
                }
                log_info_lazy!("EXC_META_PTR_DUMP: addr=0x{:x} bytes={}", ptr, ptr_bytes);
                let _ = log_exception_object_dump(ptr as *mut c_void, ptr_bytes);
            }
        }
        let dump_tinfo = env_utils::env_truthy(EXC_DUMP_TINFO_ENV);
        if dump_tinfo {
            log_info_lazy!("EXC_META_TINFO_DUMP: addr=0x{:x} bytes=256", tinfo as usize);
            let _ = log_exception_object_dump(tinfo as *mut c_void, 256);
        }
        if env_utils::env_truthy(EXC_DUMP_SCAN_STRINGS_ENV) {
            let scan_bytes = env_usize(EXC_DUMP_SCAN_STRINGS_BYTES_ENV).unwrap_or(2048);
            log_info_lazy!(
                "EXC_META_SCAN: addr=0x{:x} bytes={}",
                thrown_exception as usize,
                scan_bytes
            );
            log_exception_string_scan(thrown_exception, scan_bytes);
        }
    }

    unsafe {
        let verbose_env = libc::getenv(b"PLEX_PG_EXCEPTION_VERBOSE\0".as_ptr() as *const c_char);
        let verbose_exceptions = !verbose_env.is_null()
            && CStr::from_ptr(verbose_env) != CStr::from_bytes_with_nul(b"0\0").unwrap();
        let nonshim_env =
            libc::getenv(b"PLEX_PG_EXCEPTION_LOG_NONSHIM_DB\0".as_ptr() as *const c_char);
        let log_nonshim_db = !nonshim_env.is_null()
            && CStr::from_ptr(nonshim_env) != CStr::from_bytes_with_nul(b"0\0").unwrap();

        let mut is_db_exception = false;
        if !type_name.is_null() {
            let n2db = libc::strstr(type_name, b"N2DB\0".as_ptr() as *const c_char);
            let db9 = libc::strstr(type_name, b"DB9Exception\0".as_ptr() as *const c_char);
            let dbxx = libc::strstr(type_name, b"DB::Exception\0".as_ptr() as *const c_char);
            is_db_exception = !n2db.is_null() || !db9.is_null() || !dbxx.is_null();
        }

        let tls_column_type_calls = *tls_column_type_calls_ptr();
        let tls_value_type_calls = *tls_value_type_calls_ptr();
        let tls_last_query = *tls_last_query_ptr();
        let this_thread_used_shim =
            tls_column_type_calls > 0 || tls_value_type_calls > 0 || !tls_last_query.is_null();

        let should_log = verbose_exceptions
            || (is_db_exception && (this_thread_used_shim || log_nonshim_db))
            || ((total_count as c_int) <= MAX_LOGGED_TOTAL
                && (tracker.is_null() || (&*tracker).count <= MAX_LOGGED_PER_TYPE)
                && this_thread_used_shim);

        let should_trace =
            is_db_exception || (!tracker.is_null() && (&*tracker).logged_with_trace == 0);

        if should_log {
            let demangled =
                rust_print_exception_info(type_name, total_count, thrown_exception, tinfo);

            if should_trace {
                if !tracker.is_null() {
                    (&mut *tracker).logged_with_trace = 1;
                }
                if is_db_exception || verbose_exceptions {
                    rust_pg_exception_dump_recent_queries();
                    rust_pg_exception_dump_recent_phases();
                }
                platform_print_backtrace(b"Exception Stack Trace\0".as_ptr() as *const c_char, 2);
            }

            write_box_line(BOX_BL, BOX_BR);
            libc::fflush(stderr_ptr());

            if !demangled.is_null() {
                libc::free(demangled as *mut c_void);
            }
        } else if (total_count as c_int) == MAX_LOGGED_TOTAL + 1 {
            libc::fprintf(stderr_ptr(), b"\n\0".as_ptr() as *const c_char);
            write_box_line(BOX_TL, BOX_TR);
            libc::fprintf(
                stderr_ptr(),
                b"\xE2\x95\x91 [THROTTLE] Exception logging limited (>%d). Summary in log file.              \xE2\x95\x91\n\0"
                    .as_ptr() as *const c_char,
                MAX_LOGGED_TOTAL,
            );
            write_box_line(BOX_BL, BOX_BR);
            libc::fflush(stderr_ptr());
        }

        *in_handler_flag = 0;
    }

    1
}

pub fn rust_pg_exception_get_last_query() -> *const c_char {
    unsafe { *tls_last_query_ptr() }
}

pub fn rust_pg_exception_get_last_column() -> *const c_char {
    let len = CRASH_LAST_COLUMN_LEN.load(Ordering::SeqCst);
    if len > 0 && (len as usize) < CRASH_COLUMN_MAX_LEN {
        ptr::addr_of!(CRASH_LAST_COLUMN) as *const c_char
    } else {
        ptr::null()
    }
}

#[no_mangle]
pub extern "C" fn common_signal_handler(sig: c_int) {
    rust_common_signal_handler(sig);
}

#[no_mangle]
pub extern "C" fn print_exception_info(
    type_name: *const c_char,
    count: c_int,
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
) -> *mut c_char {
    rust_print_exception_info(type_name, count, thrown_exception, tinfo)
}

#[no_mangle]
pub extern "C" fn common_handle_exception(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
    in_handler_flag: *mut c_int,
    should_call_original: *mut c_int,
) -> c_int {
    rust_common_handle_exception(
        thrown_exception,
        tinfo,
        in_handler_flag,
        should_call_original,
    )
}

#[no_mangle]
pub extern "C" fn pg_exception_get_last_query() -> *const c_char {
    rust_pg_exception_get_last_query()
}

#[no_mangle]
pub extern "C" fn pg_exception_get_last_column() -> *const c_char {
    rust_pg_exception_get_last_column()
}
