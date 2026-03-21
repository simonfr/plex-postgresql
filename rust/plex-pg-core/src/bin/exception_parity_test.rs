#![cfg(target_os = "macos")]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::ptr;
use plex_pg_core::exception_what::{pg_exception_extract_what, pg_exception_install_terminate_logger};

extern "C" {
}

#[repr(C)]
struct FakeStdException {
    vtable: *const c_void,
    msg: *const c_char,
}

static mut VTABLE_STORAGE: [*const c_void; 3] = [ptr::null(), ptr::null(), ptr::null()];

extern "C" fn fake_what(this: *const c_void) -> *const c_char {
    unsafe {
        let ex = this as *const FakeStdException;
        (*ex).msg
    }
}

fn ensure_vtable() -> *const c_void {
    unsafe {
        if VTABLE_STORAGE[2].is_null() {
            VTABLE_STORAGE[2] = fake_what as *const c_void;
        }
        std::ptr::addr_of!(VTABLE_STORAGE) as *const c_void
    }
}

fn get_std_exception_tinfo() -> *mut c_void {
    unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, b"_ZTISt9exception\0".as_ptr() as *const c_char);
        if !sym.is_null() {
            return sym as *mut c_void;
        }
        let sym = libc::dlsym(
            libc::RTLD_DEFAULT,
            b"_ZTISt3__19exception\0".as_ptr() as *const c_char,
        );
        sym as *mut c_void
    }
}

fn run(trigger_terminate: bool) -> i32 {
    unsafe {
        std::env::set_var("PLEX_PG_EXCEPTION_ASSUME_OBJECT", "1");
        pg_exception_install_terminate_logger();

        let msg = CString::new("rust-exception-what").unwrap();
        let obj = Box::new(FakeStdException {
            vtable: ensure_vtable(),
            msg: msg.as_ptr(),
        });
        let raw = Box::into_raw(obj) as *mut c_void;

        let tinfo = get_std_exception_tinfo();
        if tinfo.is_null() {
            eprintln!("error: failed to resolve std::exception typeinfo");
            drop(Box::from_raw(raw as *mut FakeStdException));
            return 4;
        }

        let mut out_buf = [0 as c_char; 128];
        let ok = pg_exception_extract_what(raw, tinfo, out_buf.as_mut_ptr(), out_buf.len());
        if ok == 0 {
            eprintln!("error: pg_exception_extract_what returned 0");
            drop(Box::from_raw(raw as *mut FakeStdException));
            return 5;
        }
        let extracted = CStr::from_ptr(out_buf.as_ptr()).to_string_lossy();
        if !extracted.contains("rust-exception-what") {
            eprintln!("error: unexpected what(): {}", extracted);
            drop(Box::from_raw(raw as *mut FakeStdException));
            return 6;
        }
        drop(Box::from_raw(raw as *mut FakeStdException));

        if trigger_terminate {
            eprintln!("terminate path skipped: requires real C++ exception object");
        }
    }

    0
}

fn main() {
    let mut trigger_terminate = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--terminate" => {
                trigger_terminate = true;
            }
            _ => {}
        }
    }

    let code = run(trigger_terminate);
    if code != 0 {
        std::process::exit(code);
    }
}
