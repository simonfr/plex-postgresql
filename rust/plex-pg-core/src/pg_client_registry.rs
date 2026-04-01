use std::collections::HashMap;
use std::sync::Mutex;

use crate::sync_utils::mutex_lock;

pub(crate) struct ConnectionRegistry {
    map: Mutex<HashMap<usize, usize>>,
}

impl ConnectionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn register(&self, db_handle: usize, conn_ptr: usize) {
        mutex_lock(&self.map).insert(db_handle, conn_ptr);
    }

    pub(crate) fn unregister(&self, db_handle: usize) -> Option<usize> {
        mutex_lock(&self.map).remove(&db_handle)
    }

    pub(crate) fn find(&self, db_handle: usize) -> Option<usize> {
        mutex_lock(&self.map).get(&db_handle).copied()
    }

    pub(crate) fn contains_conn(&self, conn_ptr: usize) -> bool {
        mutex_lock(&self.map).values().any(|&conn| conn == conn_ptr)
    }

    pub(crate) fn find_any(&self, predicate: impl Fn(usize) -> bool) -> Option<usize> {
        mutex_lock(&self.map)
            .values()
            .copied()
            .find(|&conn| predicate(conn))
    }

    pub(crate) fn find_any_library(&self, is_library: impl Fn(usize) -> bool) -> Option<usize> {
        self.find_any(is_library)
    }

    pub(crate) fn clear(&self) {
        mutex_lock(&self.map).clear();
    }

    pub(crate) fn drain_all(&self) -> Vec<usize> {
        let mut map = mutex_lock(&self.map);
        let conns: Vec<usize> = map.values().copied().collect();
        map.clear();
        conns
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        mutex_lock(&self.map).len()
    }
}

pub(crate) struct DbToPool {
    map: Mutex<HashMap<usize, usize>>,
}

impl DbToPool {
    pub(crate) fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn assign(&self, db_handle: usize, slot_index: usize) {
        mutex_lock(&self.map).insert(db_handle, slot_index);
    }

    pub(crate) fn release(&self, db_handle: usize) -> Option<usize> {
        mutex_lock(&self.map).remove(&db_handle)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn find(&self, db_handle: usize) -> Option<usize> {
        mutex_lock(&self.map).get(&db_handle).copied()
    }

    pub(crate) fn clear(&self) {
        mutex_lock(&self.map).clear();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        mutex_lock(&self.map).len()
    }
}
