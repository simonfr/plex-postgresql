#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    Free,
    InUse,
    Idle,
}

#[derive(Clone, Debug)]
struct Slot {
    state: SlotState,
    last_owner: Option<usize>,
}

struct Pool {
    slots: Vec<Slot>,
}

impl Pool {
    fn new(size: usize) -> Self {
        Self {
            slots: vec![
                Slot {
                    state: SlotState::Free,
                    last_owner: None,
                };
                size
            ],
        }
    }

    fn acquire_thread_mode(&mut self, thread_id: usize) -> Option<usize> {
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.state == SlotState::Free {
                slot.state = SlotState::InUse;
                slot.last_owner = Some(thread_id);
                return Some(idx);
            }
        }
        None
    }

    fn acquire_borrow(&mut self) -> Option<usize> {
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.state == SlotState::Free {
                slot.state = SlotState::InUse;
                return Some(idx);
            }
        }
        None
    }

    fn release_borrow(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx) {
            slot.state = SlotState::Free;
        }
    }

    fn release_idle(&mut self, idx: usize, thread_id: usize) {
        if let Some(slot) = self.slots.get_mut(idx) {
            slot.state = SlotState::Idle;
            slot.last_owner = Some(thread_id);
        }
    }

    fn acquire_idle(&mut self, thread_id: usize) -> (Option<usize>, bool) {
        // Fast path: reclaim own idle slot
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.state == SlotState::Idle && slot.last_owner == Some(thread_id) {
                slot.state = SlotState::InUse;
                return (Some(idx), true);
            }
        }
        // Slow path: any free or idle slot
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.state == SlotState::Free || slot.state == SlotState::Idle {
                slot.state = SlotState::InUse;
                slot.last_owner = Some(thread_id);
                return (Some(idx), false);
            }
        }
        (None, false)
    }
}

#[test]
fn thread_mode_locks_out_excess_threads() {
    let mut pool = Pool::new(2);
    let mut acquired = 0;
    let mut locked_out = 0;

    for tid in 0..4 {
        if pool.acquire_thread_mode(tid).is_some() {
            acquired += 1;
        } else {
            locked_out += 1;
        }
    }

    assert_eq!(acquired, 2);
    assert_eq!(locked_out, 2);
}

#[test]
fn borrow_mode_allows_progress_for_all_threads() {
    let mut pool = Pool::new(2);
    let mut successes = 0;

    for _tid in 0..4 {
        if let Some(idx) = pool.acquire_borrow() {
            successes += 1;
            pool.release_borrow(idx);
        }
    }

    assert_eq!(successes, 4);
}

#[test]
fn idle_mode_fast_path_hits() {
    let mut pool = Pool::new(2);
    let (idx, fast) = pool.acquire_idle(1);
    assert!(idx.is_some());
    assert!(!fast);

    let idx = idx.unwrap();
    pool.release_idle(idx, 1);

    let (_idx2, fast2) = pool.acquire_idle(1);
    assert!(fast2, "expected fast-path hit for same thread");
}
