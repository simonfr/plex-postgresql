use super::*;
use std::ffi::CStr;
use std::sync::Mutex;

const MAX_EXCEPTION_TYPES: usize = 64;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ExceptionTypeTracker {
    pub(crate) type_name: *const c_char,
    pub(crate) count: c_int,
    pub(crate) logged_with_trace: c_int,
}

// Safety: ExceptionTypeTracker contains *const c_char pointers that point to
// type_info names with static lifetime (from C++ RTTI). These are valid for the
// lifetime of the process and safe to access from any thread.
unsafe impl Send for ExceptionTypeTracker {}

pub(crate) struct ExceptionTrackerState {
    pub(crate) types: [ExceptionTypeTracker; MAX_EXCEPTION_TYPES],
    pub(crate) count: c_int,
}

unsafe impl Send for ExceptionTrackerState {}

pub(crate) static EXCEPTION_TRACKER: Mutex<ExceptionTrackerState> =
    Mutex::new(ExceptionTrackerState {
        types: [ExceptionTypeTracker {
            type_name: ptr::null(),
            count: 0,
            logged_with_trace: 0,
        }; MAX_EXCEPTION_TYPES],
        count: 0,
    });

pub(super) unsafe fn get_exception_tracker_impl(
    type_name: *const c_char,
) -> *mut ExceptionTypeTracker {
    let mut guard = EXCEPTION_TRACKER.lock().unwrap_or_else(|e| e.into_inner());

    for i in 0..guard.count {
        let tracker = &mut guard.types[i as usize] as *mut ExceptionTypeTracker;
        let tracker_ref = &mut *tracker;
        if tracker_ref.type_name == type_name
            || (!tracker_ref.type_name.is_null()
                && !type_name.is_null()
                && CStr::from_ptr(tracker_ref.type_name) == CStr::from_ptr(type_name))
        {
            tracker_ref.count += 1;
            return tracker;
        }
    }

    let idx = guard.count as usize;
    if idx < MAX_EXCEPTION_TYPES {
        let tracker = &mut guard.types[idx] as *mut ExceptionTypeTracker;
        let t = &mut *tracker;
        t.type_name = type_name;
        t.count = 1;
        t.logged_with_trace = 0;
        guard.count += 1;
        return tracker;
    }

    ptr::null_mut()
}

pub(super) fn reset_exception_tracking_impl() {
    total_exception_count.store(0, Ordering::SeqCst);
    if let Ok(mut guard) = EXCEPTION_TRACKER.lock() {
        guard.count = 0;
    }
}
