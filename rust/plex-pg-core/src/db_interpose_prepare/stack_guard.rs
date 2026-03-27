use super::*;
use crate::log_info_lazy;

pub(super) struct PrepareDepthGuard {
    active: bool,
}

impl PrepareDepthGuard {
    pub(super) unsafe fn enter() -> Self {
        let depth = tls_prepare_v2_depth_ptr();
        *depth += 1;
        Self { active: true }
    }

    pub(super) unsafe fn decrement_now(&mut self) {
        if self.active {
            let depth = tls_prepare_v2_depth_ptr();
            *depth -= 1;
            self.active = false;
        }
    }
}

impl Drop for PrepareDepthGuard {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                let depth = tls_prepare_v2_depth_ptr();
                *depth -= 1;
            }
        }
    }
}

pub(super) unsafe fn log_stack_info(stack_size: isize, stack_used: isize, stack_remaining: isize) {
    STACK_LOG_COUNTER.with(|c| {
        let cur = c.get().wrapping_add(1);
        c.set(cur);
        if cur == 1 || cur % 1000 == 0 {
            log_info_lazy!(
                "STACK_CHECK: size={}KB used={}KB remaining={}KB (threshold=64KB)",
                stack_size / 1024,
                stack_used / 1024,
                stack_remaining / 1024
            );
        }
    });
}
