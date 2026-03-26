#[derive(Debug)]
struct AutoPool {
    size: usize,
    max: usize,
    used: usize,
}

impl AutoPool {
    fn new(size: usize, max: usize) -> Self {
        Self { size, max, used: 0 }
    }

    fn acquire(&mut self) -> bool {
        if self.used < self.size {
            self.used += 1;
            return true;
        }
        if self.size < self.max {
            self.size += 1;
            self.used += 1;
            return true;
        }
        false
    }

    fn release(&mut self) {
        if self.used > 0 {
            self.used -= 1;
        }
    }

    fn shrink_to(&mut self, target: usize) {
        let min_size = self.used.max(1);
        if target < min_size {
            self.size = min_size;
        } else {
            self.size = target;
        }
    }
}

#[test]
fn autogrow_and_shrink_phases() {
    let mut pool = AutoPool::new(2, 5);

    // Phase 1: grow to meet demand
    for _ in 0..4 {
        assert!(pool.acquire());
    }
    assert_eq!(pool.size, 4);
    assert_eq!(pool.used, 4);

    // Phase 2: release half, then shrink
    pool.release();
    pool.release();
    assert_eq!(pool.used, 2);
    pool.shrink_to(2);
    assert_eq!(pool.size, 2);

    // Phase 3: grow again for new demand (pool.used=2, pool.size=2)
    // First acquire fits, next two trigger auto-grow
    assert!(pool.acquire()); // used=3, size grows to 3
    assert_eq!(pool.size, 3);
    assert_eq!(pool.used, 3);

    // Phase 4: release all
    for _ in 0..3 {
        pool.release();
    }
    assert_eq!(pool.used, 0);
    pool.shrink_to(1);
    assert_eq!(pool.size, 1);

    // Phase 5: grow from minimum to max
    for _ in 0..5 {
        assert!(pool.acquire());
    }
    assert_eq!(pool.size, 5);
    assert_eq!(pool.used, 5);
    // max reached — next acquire fails
    assert!(!pool.acquire());
}
