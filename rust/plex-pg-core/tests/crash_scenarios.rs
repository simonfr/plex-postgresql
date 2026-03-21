const MAX_FAKE_VALUES: u32 = 256;

fn buggy_get_slot_signed(counter: &mut i32) -> i32 {
    let slot = *counter % (MAX_FAKE_VALUES as i32);
    *counter = counter.wrapping_add(1);
    slot
}

fn fixed_get_slot_unsigned(counter: &mut u32) -> u32 {
    let slot = *counter & (MAX_FAKE_VALUES - 1);
    *counter = counter.wrapping_add(1);
    slot
}

#[test]
fn integer_overflow_signed_can_go_negative() {
    let mut counter = i32::MAX - 5;
    let mut negative = false;
    for _ in 0..20 {
        let slot = buggy_get_slot_signed(&mut counter);
        if slot < 0 {
            negative = true;
            break;
        }
    }
    assert!(negative, "expected signed overflow to yield negative slot");
}

#[test]
fn integer_overflow_unsigned_stays_in_range() {
    let mut counter = u32::MAX - 5;
    for _ in 0..20 {
        let slot = fixed_get_slot_unsigned(&mut counter);
        assert!(slot < MAX_FAKE_VALUES);
    }
}

#[test]
fn bitmask_matches_modulo_for_power_of_two() {
    for i in 0..1000u32 {
        assert_eq!(i & 0xFF, i % 256);
    }

    let edge = [0u32, 255, 256, 257, u32::MAX - 1, u32::MAX];
    for v in edge {
        assert_eq!(v & 0xFF, v % 256);
    }
}

const RECURSION_LIMIT: i32 = 100;
const STACK_HARD_LIMIT: usize = 400_000;
const STACK_SOFT_LIMIT: usize = 500_000;

#[derive(Default)]
struct ProtectionResult {
    recursion_rejected: bool,
    stack_rejected: bool,
    soft_limit_triggered: bool,
}

fn simulate_prepare_with_protection(depth: i32, stack_remaining: usize) -> ProtectionResult {
    let mut result = ProtectionResult::default();

    if depth > RECURSION_LIMIT {
        result.recursion_rejected = true;
        return result;
    }

    if stack_remaining < STACK_HARD_LIMIT {
        result.stack_rejected = true;
        return result;
    }

    if stack_remaining < STACK_SOFT_LIMIT {
        result.soft_limit_triggered = true;
    }

    result
}

#[test]
fn recursion_limit_rejects_over_limit() {
    let r = simulate_prepare_with_protection(218, 1_000_000);
    assert!(r.recursion_rejected);
}

#[test]
fn recursion_limit_boundary() {
    let r_ok = simulate_prepare_with_protection(100, 1_000_000);
    let r_fail = simulate_prepare_with_protection(101, 1_000_000);
    assert!(!r_ok.recursion_rejected);
    assert!(r_fail.recursion_rejected);
}

#[test]
fn stack_hard_limit_rejects_low_remaining() {
    let ok = simulate_prepare_with_protection(1, 450_000);
    let fail = simulate_prepare_with_protection(1, 350_000);
    assert!(!ok.stack_rejected);
    assert!(fail.stack_rejected);
}

#[test]
fn stack_soft_limit_triggers_warning() {
    let soft = simulate_prepare_with_protection(1, 450_000);
    let ok = simulate_prepare_with_protection(1, 800_000);
    assert!(soft.soft_limit_triggered);
    assert!(!ok.soft_limit_triggered);
}
