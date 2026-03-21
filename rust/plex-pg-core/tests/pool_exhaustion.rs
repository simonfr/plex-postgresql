use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct Pool {
    slots: Mutex<Vec<bool>>, // true = in use
}

impl Pool {
    fn new(size: usize) -> Self {
        Self {
            slots: Mutex::new(vec![false; size]),
        }
    }

    fn acquire(&self, timeout: Duration) -> Option<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut slots = self.slots.lock().unwrap();
                if let Some((idx, slot)) = slots.iter_mut().enumerate().find(|(_, s)| !**s) {
                    *slot = true;
                    return Some(idx);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn release(&self, idx: usize) {
        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(idx) {
            *slot = false;
        }
    }
}

#[test]
fn pool_exhaustion_times_out_under_pressure() {
    let pool = Arc::new(Pool::new(2));
    let timeouts = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for tid in 0..4 {
        let pool = pool.clone();
        let timeouts = timeouts.clone();
        handles.push(thread::spawn(move || {
            if let Some(idx) = pool.acquire(Duration::from_millis(30)) {
                // Hold the slot longer for the first two threads.
                if tid < 2 {
                    thread::sleep(Duration::from_millis(80));
                }
                pool.release(idx);
            } else {
                timeouts.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for h in handles {
        let _ = h.join();
    }

    let total_timeouts = timeouts.load(Ordering::Relaxed);
    assert!(total_timeouts >= 1, "expected at least one timeout under exhaustion");
}
