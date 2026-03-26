#![cfg(target_os = "linux")]

use std::mem;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::db_interpose_common::stderr_ptr;
use crate::env_utils;

type SetsockoptFn =
    unsafe extern "C" fn(c_int, c_int, c_int, *const c_void, libc::socklen_t) -> c_int;

static mut ORIG_SETSOCKOPT: Option<SetsockoptFn> = None;
static PMS_NET_COMPAT_ENABLED: AtomicI32 = AtomicI32::new(0);
static PMS_NET_COMPAT_LOG_BUDGET: AtomicI32 = AtomicI32::new(0);

const DEFAULT_LOG_BUDGET: i32 = 16;

unsafe fn resolve_setsockopt() -> Option<SetsockoptFn> {
    if let Some(f) = std::ptr::read(std::ptr::addr_of!(ORIG_SETSOCKOPT)) {
        return Some(f);
    }

    let sym = libc::dlsym(libc::RTLD_NEXT, b"setsockopt\0".as_ptr() as *const c_char);
    if sym.is_null() {
        return None;
    }

    let f: SetsockoptFn = mem::transmute::<*mut c_void, SetsockoptFn>(sym);
    std::ptr::write(std::ptr::addr_of_mut!(ORIG_SETSOCKOPT), Some(f));
    Some(f)
}

fn is_enabled() -> bool {
    PMS_NET_COMPAT_ENABLED.load(Ordering::Acquire) != 0
}

pub(crate) fn disable_for_fork_child_fast() {
    PMS_NET_COMPAT_ENABLED.store(0, Ordering::Relaxed);
    PMS_NET_COMPAT_LOG_BUDGET.store(0, Ordering::Relaxed);
}

pub fn configure_from_env() {
    let enabled = !env_utils::env_truthy(b"PLEX_PG_DISABLE_PMS_NET_COMPAT\0");
    PMS_NET_COMPAT_ENABLED.store(if enabled { 1 } else { 0 }, Ordering::Release);
    let budget = env_utils::env_usize(b"PLEX_PG_PMS_NET_COMPAT_LOG_LIMIT\0")
        .map(|v| v.min(i32::MAX as usize) as i32)
        .unwrap_or(DEFAULT_LOG_BUDGET);
    PMS_NET_COMPAT_LOG_BUDGET.store(budget.max(0), Ordering::Release);

    unsafe {
        let _ = libc::fprintf(
            stderr_ptr(),
            if enabled {
                b"[SHIM_INIT] PMS net compat ENABLED (mask multicast setsockopt EADDRNOTAVAIL/ENODEV on UDP sockets)\n\0"
                    .as_ptr() as *const c_char
            } else {
                b"[SHIM_INIT] PMS net compat DISABLED via PLEX_PG_DISABLE_PMS_NET_COMPAT\n\0"
                    .as_ptr() as *const c_char
            },
        );
        let _ = libc::fflush(stderr_ptr());
    }
}

fn is_multicast_option(level: c_int, optname: c_int) -> bool {
    level == libc::IPPROTO_IP
        && matches!(
            optname,
            libc::IP_ADD_MEMBERSHIP | libc::IP_DROP_MEMBERSHIP | libc::IP_MULTICAST_IF
        )
}

fn is_swallowable_errno(err: c_int) -> bool {
    err == libc::EADDRNOTAVAIL || err == libc::ENODEV || err == libc::EOPNOTSUPP
}

unsafe fn is_udp_socket(sockfd: c_int) -> bool {
    let mut sock_type: c_int = 0;
    let mut len = mem::size_of::<c_int>() as libc::socklen_t;
    libc::getsockopt(
        sockfd,
        libc::SOL_SOCKET,
        libc::SO_TYPE,
        &mut sock_type as *mut c_int as *mut c_void,
        &mut len,
    ) == 0
        && sock_type == libc::SOCK_DGRAM
}

unsafe fn socket_local_port(sockfd: c_int) -> c_int {
    let mut addr: libc::sockaddr_storage = mem::zeroed();
    let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if libc::getsockname(
        sockfd,
        &mut addr as *mut libc::sockaddr_storage as *mut libc::sockaddr,
        &mut len,
    ) != 0
    {
        return -1;
    }

    match addr.ss_family as c_int {
        libc::AF_INET => {
            let sa = &*((&addr as *const libc::sockaddr_storage) as *const libc::sockaddr_in);
            u16::from_be(sa.sin_port) as c_int
        }
        libc::AF_INET6 => {
            let sa = &*((&addr as *const libc::sockaddr_storage) as *const libc::sockaddr_in6);
            u16::from_be(sa.sin6_port) as c_int
        }
        _ => -1,
    }
}

unsafe fn set_errno(err: c_int) {
    *libc::__errno_location() = err;
}

unsafe fn clear_errno() {
    set_errno(0);
}

unsafe fn maybe_log_swallow(sockfd: c_int, level: c_int, optname: c_int, err: c_int) {
    let budget = PMS_NET_COMPAT_LOG_BUDGET.load(Ordering::Relaxed);
    if budget <= 0 {
        return;
    }
    PMS_NET_COMPAT_LOG_BUDGET.fetch_sub(1, Ordering::Relaxed);

    let local_port = socket_local_port(sockfd);
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[PMS_NET_COMPAT] swallowed setsockopt(fd=%d port=%d level=%d opt=%d errno=%d) to keep PMS discovery startup non-fatal\n\0"
            .as_ptr() as *const c_char,
        sockfd,
        local_port,
        level,
        optname,
        err,
    );
    let _ = libc::fflush(stderr_ptr());
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `setsockopt`. Callers must obey libc
/// preconditions for the provided socket descriptor and option pointers.
pub unsafe extern "C" fn setsockopt(
    sockfd: c_int,
    level: c_int,
    optname: c_int,
    optval: *const c_void,
    optlen: libc::socklen_t,
) -> c_int {
    let Some(orig) = resolve_setsockopt() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    let rc = orig(sockfd, level, optname, optval, optlen);
    if rc == 0 || !is_enabled() {
        return rc;
    }

    let err = *libc::__errno_location();
    if !is_swallowable_errno(err) || !is_multicast_option(level, optname) || !is_udp_socket(sockfd)
    {
        return rc;
    }

    maybe_log_swallow(sockfd, level, optname, err);
    clear_errno();
    0
}

#[cfg(test)]
mod tests {
    use super::{is_multicast_option, is_swallowable_errno};

    #[test]
    fn multicast_options_are_narrow() {
        assert!(is_multicast_option(
            libc::IPPROTO_IP,
            libc::IP_ADD_MEMBERSHIP
        ));
        assert!(is_multicast_option(
            libc::IPPROTO_IP,
            libc::IP_DROP_MEMBERSHIP
        ));
        assert!(is_multicast_option(libc::IPPROTO_IP, libc::IP_MULTICAST_IF));
        assert!(!is_multicast_option(
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_LOOP
        ));
        assert!(!is_multicast_option(libc::SOL_SOCKET, libc::SO_REUSEADDR));
    }

    #[test]
    fn swallowable_errors_match_runtime_compat_scope() {
        assert!(is_swallowable_errno(libc::EADDRNOTAVAIL));
        assert!(is_swallowable_errno(libc::ENODEV));
        assert!(is_swallowable_errno(libc::EOPNOTSUPP));
        assert!(!is_swallowable_errno(libc::EINVAL));
        assert!(!is_swallowable_errno(libc::EBADF));
    }
}
