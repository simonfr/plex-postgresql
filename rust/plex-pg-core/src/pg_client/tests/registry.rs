use super::*;

// ═════════════════════════════════════════════════════════════════════════
// Connection Registry
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn registry_register_and_find() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    assert_eq!(reg.find(0x100), Some(0xAAA));
}

#[test]
fn registry_find_missing_returns_none() {
    let reg = ConnectionRegistry::new();
    assert_eq!(reg.find(0x100), None);
}

#[test]
fn registry_unregister_removes() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    assert_eq!(reg.unregister(0x100), Some(0xAAA));
    assert_eq!(reg.find(0x100), None);
}

#[test]
fn registry_unregister_missing_returns_none() {
    let reg = ConnectionRegistry::new();
    assert_eq!(reg.unregister(0x100), None);
}

#[test]
fn registry_multiple_entries() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    reg.register(0x200, 0xBBB);
    reg.register(0x300, 0xCCC);
    assert_eq!(reg.find(0x100), Some(0xAAA));
    assert_eq!(reg.find(0x200), Some(0xBBB));
    assert_eq!(reg.find(0x300), Some(0xCCC));
    assert_eq!(reg.len(), 3);
}

#[test]
fn registry_overwrite_existing() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    reg.register(0x100, 0xBBB);
    assert_eq!(reg.find(0x100), Some(0xBBB));
    assert_eq!(reg.len(), 1);
}

#[test]
fn registry_clear_empties_all() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    reg.register(0x200, 0xBBB);
    reg.clear();
    assert_eq!(reg.len(), 0);
    assert_eq!(reg.find(0x100), None);
}

#[test]
fn registry_find_any_library() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    reg.register(0x200, 0xBBB);
    // Predicate: "is library" if conn addr is 0xBBB
    let result = reg.find_any_library(|conn| conn == 0xBBB);
    assert_eq!(result, Some(0xBBB));
}

#[test]
fn registry_find_any_library_none_match() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    let result = reg.find_any_library(|_| false);
    assert_eq!(result, None);
}

#[test]
fn registry_find_any() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    reg.register(0x200, 0xBBB);
    let result = reg.find_any(|conn| conn == 0xAAA);
    assert_eq!(result, Some(0xAAA));
}

#[test]
fn registry_find_any_none_match() {
    let reg = ConnectionRegistry::new();
    reg.register(0x100, 0xAAA);
    let result = reg.find_any(|_| false);
    assert_eq!(result, None);
}
