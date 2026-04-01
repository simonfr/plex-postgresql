use super::*;
use crate::log_info_lazy;

pub(super) const BOX_INNER_WIDTH: usize = 78;
pub(super) const BOX_TL: &[u8] = b"\xE2\x95\x94"; // ╔
pub(super) const BOX_TR: &[u8] = b"\xE2\x95\x97"; // ╗
pub(super) const BOX_BL: &[u8] = b"\xE2\x95\x9A"; // ╚
pub(super) const BOX_BR: &[u8] = b"\xE2\x95\x9D"; // ╝
pub(super) const BOX_H: &[u8] = b"\xE2\x95\x90"; // ═
pub(super) const BOX_ML: &[u8] = b"\xE2\x95\xA0"; // ╠
pub(super) const BOX_MR: &[u8] = b"\xE2\x95\xA3"; // ╣

pub(super) const TRACE_LAST_QUERY_DEFAULT: &[u8] = b"/tmp/plex_pg_last_query.log\0";

pub(super) fn env_usize(name: &[u8]) -> Option<usize> {
    env_utils::env_usize(name)
}

#[cfg(target_os = "linux")]
unsafe fn read_process_memory(addr: *const c_void, buf: &mut [u8]) -> isize {
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut c_void,
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: addr as *mut c_void,
        iov_len: buf.len(),
    };
    libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0)
}

#[cfg(not(target_os = "linux"))]
unsafe fn read_process_memory(_addr: *const c_void, _buf: &mut [u8]) -> isize {
    -1
}

pub(super) fn log_exception_object_dump(thrown_exception: *mut c_void, bytes: usize) -> Vec<usize> {
    if bytes == 0 {
        return Vec::new();
    }
    let max_bytes = bytes.min(1024);
    let mut buf = vec![0u8; max_bytes];
    let n = unsafe { read_process_memory(thrown_exception, &mut buf) };
    if n <= 0 {
        log_info_lazy!(
            "EXC_META_DUMP: read failed addr=0x{:x} bytes={}",
            thrown_exception as usize,
            max_bytes
        );
        return Vec::new();
    }
    let used = n as usize;
    log_info_lazy!(
        "EXC_META_DUMP: addr=0x{:x} bytes={}",
        thrown_exception as usize,
        used
    );

    let data = &buf[..used];
    let mut pointers: Vec<usize> = Vec::new();
    let mut ptr_count = 0usize;
    let word = std::mem::size_of::<usize>();
    let aligned_len = data.len().saturating_sub(data.len() % word);
    for offset in (0..aligned_len).step_by(word) {
        let mut raw = [0u8; std::mem::size_of::<usize>()];
        raw.copy_from_slice(&data[offset..offset + word]);
        let val = usize::from_le_bytes(raw);
        if val == 0 {
            continue;
        }
        let looks_canonical = (val >> 48) == 0 || (val >> 48) == 0xffff;
        let aligned = (val & 0x7) == 0;
        if looks_canonical && aligned {
            ptr_count += 1;
            pointers.push(val);
            if ptr_count >= 32 {
                break;
            }
        }
    }
    for (i, chunk) in data.chunks(16).enumerate() {
        let mut hex = String::with_capacity(16 * 3);
        let mut ascii = String::with_capacity(16);
        for &b in chunk {
            hex.push_str(&format!("{:02x} ", b));
            let ch = if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            };
            ascii.push(ch);
        }
        log_info_lazy!(
            "EXC_META_DUMP: +0x{:04x} {:<48} |{}|",
            i * 16,
            hex.trim_end(),
            ascii
        );
    }

    let mut sequences = 0usize;
    let mut start: Option<usize> = None;
    for (idx, &b) in data.iter().enumerate() {
        let printable = (0x20..=0x7e).contains(&b);
        if printable {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(s) = start.take() {
            let len = idx - s;
            if len >= 8 {
                let seq = String::from_utf8_lossy(&data[s..idx]).to_string();
                log_info_lazy!("EXC_META_STR: +0x{:04x} len={} '{}'", s, len, seq);
                sequences += 1;
                if sequences >= 8 {
                    log_info("EXC_META_STR: truncated (limit 8)");
                    break;
                }
            }
        }
    }
    if sequences < 8 {
        if let Some(s) = start {
            let len = data.len().saturating_sub(s);
            if len >= 8 {
                let seq = String::from_utf8_lossy(&data[s..]).to_string();
                log_info_lazy!("EXC_META_STR: +0x{:04x} len={} '{}'", s, len, seq);
            }
        }
    }
    if !pointers.is_empty() {
        let mut msg = String::from("EXC_META_PTRS:");
        for ptr in &pointers {
            msg.push_str(&format!(" 0x{:x}", ptr));
        }
        log_info(&msg);
    }
    pointers
}

pub(super) fn log_exception_string_scan(base: *mut c_void, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let max_bytes = bytes.min(4096);
    let mut buf = vec![0u8; max_bytes];
    let n = unsafe { read_process_memory(base, &mut buf) };
    if n <= 0 {
        log_info_lazy!(
            "EXC_META_SCAN: read failed addr=0x{:x} bytes={}",
            base as usize,
            max_bytes
        );
        return;
    }
    let used = n as usize;
    let data = &buf[..used];
    let mut sequences = 0usize;
    let mut start: Option<usize> = None;
    for (idx, &b) in data.iter().enumerate() {
        let printable = (0x20..=0x7e).contains(&b);
        if printable {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(s) = start.take() {
            let len = idx - s;
            if len >= 12 {
                let seq = String::from_utf8_lossy(&data[s..idx]).to_string();
                log_info_lazy!("EXC_META_SCAN: +0x{:04x} len={} '{}'", s, len, seq);
                sequences += 1;
                if sequences >= 12 {
                    log_info("EXC_META_SCAN: truncated (limit 12)");
                    break;
                }
            }
        }
    }
    if sequences < 12 {
        if let Some(s) = start {
            let len = data.len().saturating_sub(s);
            if len >= 12 {
                let seq = String::from_utf8_lossy(&data[s..]).to_string();
                log_info_lazy!("EXC_META_SCAN: +0x{:04x} len={} '{}'", s, len, seq);
            }
        }
    }
}

pub(super) fn write_box_line(left: &[u8], right: &[u8]) {
    unsafe {
        let fd = libc::STDERR_FILENO;
        let _ = libc::write(fd, left.as_ptr() as *const c_void, left.len());
        for _ in 0..BOX_INNER_WIDTH {
            let _ = libc::write(fd, BOX_H.as_ptr() as *const c_void, BOX_H.len());
        }
        let _ = libc::write(fd, right.as_ptr() as *const c_void, right.len());
        let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
    }
}

pub(super) fn trace_last_query_enabled() -> bool {
    // `get_or_init` is thread-safe; the old `TRACE_LAST_QUERY_CACHED` atomic
    // is no longer needed because `OnceLock` provides the same semantics
    // without a separate flag.
    TRACE_LAST_QUERY_PATH
        .get_or_init(|| unsafe {
            let env = libc::getenv(b"PLEX_PG_TRACE_LAST_QUERY\0".as_ptr() as *const c_char);
            if !env.is_null() && *env != 0 && *env != b'0' as c_char {
                let path =
                    libc::getenv(b"PLEX_PG_TRACE_LAST_QUERY_FILE\0".as_ptr() as *const c_char);
                if !path.is_null() && *path != 0 {
                    SendCharPtr(path)
                } else {
                    SendCharPtr(TRACE_LAST_QUERY_DEFAULT.as_ptr() as *const c_char)
                }
            } else {
                // Feature disabled — store a null sentinel.
                SendCharPtr(ptr::null())
            }
        })
        .0
        != ptr::null()
}
