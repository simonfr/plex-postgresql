use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::db_interpose_common::cxa_demangle_fn;

const MAX_FRAMES: usize = 64;
const MAX_DISPLAY: usize = 25;
const MAX_FUNC_LEN: usize = 70;

#[repr(C)]
#[derive(Copy, Clone)]
struct ResolvedSymbol {
    func: [c_char; 256],
    lib: [c_char; 256],
}

impl Default for ResolvedSymbol {
    fn default() -> Self {
        ResolvedSymbol {
            func: [0; 256],
            lib: [0; 256],
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn backtrace(frames: *mut *mut c_void, size: c_int) -> c_int;
    fn backtrace_symbols(frames: *const *mut c_void, size: c_int) -> *mut *mut c_char;
}

#[cfg(target_os = "linux")]
#[cfg(target_arch = "x86_64")]
unsafe fn current_frame_ptr() -> *mut *mut c_void {
    let fp: *mut *mut c_void;
    core::arch::asm!("mov {}, rbp", out(reg) fp);
    fp
}

#[cfg(target_os = "linux")]
#[cfg(target_arch = "aarch64")]
unsafe fn current_frame_ptr() -> *mut *mut c_void {
    let fp: *mut *mut c_void;
    core::arch::asm!("mov {}, x29", out(reg) fp);
    fp
}

#[cfg(target_os = "linux")]
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
unsafe fn current_frame_ptr() -> *mut *mut c_void {
    ptr::null_mut()
}

#[cfg(target_os = "macos")]
unsafe fn collect_frames(frames: &mut [*mut c_void]) -> usize {
    backtrace(frames.as_mut_ptr(), frames.len() as c_int) as usize
}

#[cfg(target_os = "linux")]
unsafe fn collect_frames(frames: &mut [*mut c_void]) -> usize {
    let mut depth = 0usize;
    let mut fp = current_frame_ptr();
    let mut iterations = 0usize;

    while !fp.is_null() && depth < frames.len() && iterations < 100 {
        iterations += 1;

        let fp_addr = fp as usize;
        if fp_addr < 0x1000 || fp_addr > 0x0000_ffff_ffff_ffff {
            break;
        }

        let ret_addr = *fp.add(1) as *mut c_void;
        if ret_addr.is_null() || (ret_addr as usize) < 0x1000 {
            break;
        }

        frames[depth] = ret_addr;
        depth += 1;

        let next_fp = *fp as *mut *mut c_void;
        if next_fp.is_null() || next_fp <= fp {
            break;
        }
        if (next_fp as usize).saturating_sub(fp_addr) > 0x100000 {
            break;
        }

        fp = next_fp;
    }

    depth
}

#[cfg(target_os = "macos")]
unsafe fn resolve_symbols(frames: &[*mut c_void], out: &mut [ResolvedSymbol]) {
    let symbols = backtrace_symbols(frames.as_ptr(), frames.len() as c_int);

    for (i, sym) in out.iter_mut().enumerate() {
        sym.func[0] = 0;
        sym.lib[0] = 0;

        if symbols.is_null() {
            continue;
        }

        let symbol_ptr = *symbols.add(i);
        if symbol_ptr.is_null() {
            continue;
        }

        let bytes = CStr::from_ptr(symbol_ptr).to_bytes();
        let plus_pos = bytes.iter().rposition(|&b| b == b'+');
        if let Some(plus) = plus_pos {
            let before_plus = &bytes[..plus];
            let start = before_plus
                .iter()
                .rposition(|&b| b == b' ')
                .map(|p| p + 1)
                .unwrap_or(0);
            let mut end = before_plus.len();
            while end > start && before_plus[end - 1] == b' ' {
                end -= 1;
            }
            if end > start {
                let mangled = &before_plus[start..end];
                let demangle_opt = unsafe { cxa_demangle_fn };
                if let Some(demangle) = demangle_opt {
                    let mut status: c_int = 0;
                    let demangled = demangle(
                        mangled.as_ptr() as *const c_char,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut status,
                    );
                    if !demangled.is_null() && status == 0 {
                        libc::strncpy(sym.func.as_mut_ptr(), demangled, sym.func.len() - 1);
                        libc::free(demangled as *mut c_void);
                    } else {
                        libc::strncpy(
                            sym.func.as_mut_ptr(),
                            symbol_ptr,
                            sym.func.len() - 1,
                        );
                    }
                } else {
                    libc::strncpy(
                        sym.func.as_mut_ptr(),
                        symbol_ptr,
                        sym.func.len() - 1,
                    );
                }
            }
        }

        if sym.func[0] == 0 {
            libc::strncpy(sym.func.as_mut_ptr(), symbol_ptr, sym.func.len() - 1);
        }
    }

    if !symbols.is_null() {
        libc::free(symbols as *mut c_void);
    }
}

#[cfg(target_os = "linux")]
#[derive(Copy, Clone)]
struct MapEntry {
    start: usize,
    end: usize,
    path: [c_char; 256],
}

#[cfg(target_os = "linux")]
impl Default for MapEntry {
    fn default() -> Self {
        MapEntry {
            start: 0,
            end: 0,
            path: [0; 256],
        }
    }
}

#[cfg(target_os = "linux")]
const MAX_MAPS_ENTRIES: usize = 256;

#[cfg(target_os = "linux")]
static mut MEMORY_MAP: [MapEntry; MAX_MAPS_ENTRIES] = [MapEntry {
    start: 0,
    end: 0,
    path: [0; 256],
}; MAX_MAPS_ENTRIES];

#[cfg(target_os = "linux")]
static mut MEMORY_MAP_COUNT: usize = 0;

#[cfg(target_os = "linux")]
unsafe fn load_memory_map() {
    MEMORY_MAP_COUNT = 0;
    let Ok(content) = std::fs::read_to_string("/proc/self/maps") else {
        return;
    };

    for line in content.lines() {
        if MEMORY_MAP_COUNT >= MAX_MAPS_ENTRIES {
            break;
        }

        let mut parts = line.split_whitespace();
        let Some(range) = parts.next() else { continue; };
        let Some((start_str, end_str)) = range.split_once('-') else { continue; };
        let Ok(start) = usize::from_str_radix(start_str, 16) else { continue; };
        let Ok(end) = usize::from_str_radix(end_str, 16) else { continue; };

        let path = parts.last().unwrap_or("[anonymous]");
        let mut entry = MapEntry::default();
        entry.start = start;
        entry.end = end;

        let bytes = path.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() && i < entry.path.len() - 1 {
            entry.path[i] = bytes[i] as c_char;
            i += 1;
        }
        entry.path[i] = 0;

        MEMORY_MAP[MEMORY_MAP_COUNT] = entry;
        MEMORY_MAP_COUNT += 1;
    }
}

#[cfg(target_os = "linux")]
unsafe fn find_lib_for_addr(addr: usize) -> *const c_char {
    for i in 0..MEMORY_MAP_COUNT {
        let entry = &MEMORY_MAP[i];
        if addr >= entry.start && addr < entry.end {
            let path_ptr = entry.path.as_ptr();
            let base = libc::strrchr(path_ptr, b'/' as c_int);
            return if base.is_null() { path_ptr } else { base.add(1) };
        }
    }
    b"[unknown]\0".as_ptr() as *const c_char
}

#[cfg(target_os = "linux")]
unsafe fn resolve_symbols(frames: &[*mut c_void], out: &mut [ResolvedSymbol]) {
    load_memory_map();

    for (i, sym) in out.iter_mut().enumerate() {
        sym.func[0] = 0;
        sym.lib[0] = 0;

        let mut info = libc::Dl_info {
            dli_fname: ptr::null(),
            dli_fbase: ptr::null_mut(),
            dli_sname: ptr::null(),
            dli_saddr: ptr::null_mut(),
        };

        if libc::dladdr(frames[i], &mut info) != 0 {
            if !info.dli_fname.is_null() {
                let base = libc::strrchr(info.dli_fname, b'/' as c_int);
                let lib_name = if base.is_null() { info.dli_fname } else { base.add(1) };
                libc::strncpy(sym.lib.as_mut_ptr(), lib_name, sym.lib.len() - 1);
            }

            if !info.dli_sname.is_null() {
                let demangle_opt = unsafe { cxa_demangle_fn };
                if let Some(demangle) = demangle_opt {
                    let mut status: c_int = 0;
                    let demangled = demangle(info.dli_sname, ptr::null_mut(), ptr::null_mut(), &mut status);
                    if !demangled.is_null() && status == 0 {
                        libc::strncpy(sym.func.as_mut_ptr(), demangled, sym.func.len() - 1);
                        libc::free(demangled as *mut c_void);
                    } else {
                        libc::strncpy(sym.func.as_mut_ptr(), info.dli_sname, sym.func.len() - 1);
                    }
                } else {
                    libc::strncpy(sym.func.as_mut_ptr(), info.dli_sname, sym.func.len() - 1);
                }
            }
        }

        if sym.lib[0] == 0 {
            let lib = find_lib_for_addr(frames[i] as usize);
            libc::strncpy(sym.lib.as_mut_ptr(), lib, sym.lib.len() - 1);
        }

        if sym.func[0] == 0 {
            let _ = libc::snprintf(
                sym.func.as_mut_ptr(),
                sym.func.len(),
                b"[%p]\0".as_ptr() as *const c_char,
                frames[i],
            );
        }
    }
}

fn write_stderr(msg: &str) {
    unsafe {
        let _ = libc::write(libc::STDERR_FILENO, msg.as_ptr() as *const c_void, msg.len());
    }
}

fn log_error(msg: &str) {
    if let Ok(cs) = std::ffi::CString::new(msg) {
        crate::pg_logging::rust_logging_write(0, cs.as_ptr());
    }
}

#[no_mangle]
pub extern "C" fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int) {
    let mut frames: [*mut c_void; MAX_FRAMES] = [ptr::null_mut(); MAX_FRAMES];
    let depth = unsafe { collect_frames(&mut frames) };

    if depth == 0 {
        write_stderr("\n  [Stack trace unavailable]\n");
        return;
    }

    let reason_str = unsafe {
        if reason.is_null() {
            "Unknown".to_string()
        } else {
            CStr::from_ptr(reason).to_string_lossy().into_owned()
        }
    };

    write_stderr("\n");
    write_stderr("╔══════════════════════════════════════════════════════════════════════════════╗\n");
    write_stderr(&format!("║ BACKTRACE: {:<67} ║\n", reason_str));
    write_stderr("╠══════════════════════════════════════════════════════════════════════════════╣\n");
    log_error(&format!("=== BACKTRACE ({}) ===", reason_str));

    let mut symbols = vec![ResolvedSymbol::default(); depth];
    unsafe {
        resolve_symbols(&frames[..depth], &mut symbols);
    }

    let mut printed = 0usize;
    let start = skip_frames.max(0) as usize;
    for i in start..depth {
        if printed >= MAX_DISPLAY {
            break;
        }

        let func = unsafe { CStr::from_ptr(symbols[i].func.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let mut func_display = func;
        if func_display.len() > MAX_FUNC_LEN {
            func_display.truncate(MAX_FUNC_LEN - 3);
            func_display.push_str("...");
        }

        let line = format!("[{:2}] {}", printed, func_display);
        write_stderr(&format!("║ {:<78} ║\n", line));
        log_error(&format!("  {}", line));
        printed += 1;
    }

    if depth > start + MAX_DISPLAY {
        write_stderr(&format!(
            "║ ... and {} more frames                                                         ║\n",
            depth - start - MAX_DISPLAY
        ));
    }
    write_stderr("╚══════════════════════════════════════════════════════════════════════════════╝\n");
}
