use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::sync::{atomic::{AtomicU64, AtomicUsize, Ordering}, Mutex, RwLock};

const CACHE_SIZE: usize = 512;
const CACHE_MASK: usize = CACHE_SIZE - 1;
const NUM_UNIQUE_QUERIES: usize = 100;

fn hash_sql(sql: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in sql.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Clone)]
struct Entry {
    hash: u64,
    idx: usize,
}

struct MutexCache {
    entries: Mutex<Vec<Option<Entry>>>,
}

impl MutexCache {
    fn new() -> Self {
        Self {
            entries: Mutex::new(vec![None; CACHE_SIZE]),
        }
    }

    fn populate(&self, hashes: &[(u64, usize)]) {
        let mut entries = self.entries.lock().unwrap();
        for (h, idx) in hashes {
            let start = (*h as usize) & CACHE_MASK;
            for i in 0..8 {
                let slot = (start + i) & CACHE_MASK;
                if entries[slot].is_none() {
                    entries[slot] = Some(Entry { hash: *h, idx: *idx });
                    break;
                }
            }
        }
    }

    fn lookup(&self, sql: &str) -> Option<usize> {
        let h = hash_sql(sql);
        let start = (h as usize) & CACHE_MASK;
        let entries = self.entries.lock().unwrap();
        for i in 0..8 {
            let slot = (start + i) & CACHE_MASK;
            match &entries[slot] {
                Some(e) if e.hash == h => return Some(e.idx),
                None => break,
                _ => {}
            }
        }
        None
    }
}

struct RwCache {
    entries: RwLock<Vec<Option<Entry>>>,
}

impl RwCache {
    fn new() -> Self {
        Self {
            entries: RwLock::new(vec![None; CACHE_SIZE]),
        }
    }

    fn populate(&self, hashes: &[(u64, usize)]) {
        let mut entries = self.entries.write().unwrap();
        for (h, idx) in hashes {
            let start = (*h as usize) & CACHE_MASK;
            for i in 0..8 {
                let slot = (start + i) & CACHE_MASK;
                if entries[slot].is_none() {
                    entries[slot] = Some(Entry { hash: *h, idx: *idx });
                    break;
                }
            }
        }
    }

    fn lookup(&self, sql: &str) -> Option<usize> {
        let h = hash_sql(sql);
        let start = (h as usize) & CACHE_MASK;
        let entries = self.entries.read().unwrap();
        for i in 0..8 {
            let slot = (start + i) & CACHE_MASK;
            match &entries[slot] {
                Some(e) if e.hash == h => return Some(e.idx),
                None => break,
                _ => {}
            }
        }
        None
    }
}

thread_local! {
    static TLS_CACHE: std::cell::RefCell<Vec<Option<Entry>>> = std::cell::RefCell::new(vec![None; CACHE_SIZE]);
}

fn tls_populate(hashes: &[(u64, usize)]) {
    TLS_CACHE.with(|cell| {
        let mut entries = cell.borrow_mut();
        for (h, idx) in hashes {
            let start = (*h as usize) & CACHE_MASK;
            for i in 0..8 {
                let slot = (start + i) & CACHE_MASK;
                if entries[slot].is_none() {
                    entries[slot] = Some(Entry { hash: *h, idx: *idx });
                    break;
                }
            }
        }
    });
}

fn tls_lookup(sql: &str) -> Option<usize> {
    let h = hash_sql(sql);
    let start = (h as usize) & CACHE_MASK;
    TLS_CACHE.with(|cell| {
        let entries = cell.borrow();
        for i in 0..8 {
            let slot = (start + i) & CACHE_MASK;
            match &entries[slot] {
                Some(e) if e.hash == h => return Some(e.idx),
                None => break,
                _ => {}
            }
        }
        None
    })
}

struct LockFreeEntry {
    hash: AtomicU64,
    idx: AtomicUsize,
}

struct LockFreeCache {
    entries: Vec<LockFreeEntry>,
}

impl LockFreeCache {
    fn new() -> Self {
        let mut entries = Vec::with_capacity(CACHE_SIZE);
        for _ in 0..CACHE_SIZE {
            entries.push(LockFreeEntry {
                hash: AtomicU64::new(0),
                idx: AtomicUsize::new(0),
            });
        }
        Self { entries }
    }

    fn populate(&self, hashes: &[(u64, usize)]) {
        for (h, idx) in hashes {
            let start = (*h as usize) & CACHE_MASK;
            for i in 0..8 {
                let slot = (start + i) & CACHE_MASK;
                if self.entries[slot].hash.load(Ordering::Acquire) == 0 {
                    self.entries[slot].hash.store(*h, Ordering::Release);
                    self.entries[slot].idx.store(idx + 1, Ordering::Release);
                    break;
                }
            }
        }
    }

    fn lookup(&self, sql: &str) -> Option<usize> {
        let h = hash_sql(sql);
        let start = (h as usize) & CACHE_MASK;
        for i in 0..8 {
            let slot = (start + i) & CACHE_MASK;
            let slot_hash = self.entries[slot].hash.load(Ordering::Acquire);
            if slot_hash == h {
                let idx = self.entries[slot].idx.load(Ordering::Acquire);
                return if idx == 0 { None } else { Some(idx - 1) };
            }
            if slot_hash == 0 {
                break;
            }
        }
        None
    }
}

fn bench_cache(c: &mut Criterion) {
    let mut queries = Vec::with_capacity(NUM_UNIQUE_QUERIES);
    for i in 0..NUM_UNIQUE_QUERIES {
        queries.push(format!("SELECT * FROM table{} WHERE id = ?", i));
    }
    let hashes: Vec<(u64, usize)> = queries
        .iter()
        .enumerate()
        .map(|(i, q)| (hash_sql(q), i))
        .collect();

    let mutex_cache = MutexCache::new();
    mutex_cache.populate(&hashes);
    let rw_cache = RwCache::new();
    rw_cache.populate(&hashes);
    tls_populate(&hashes);
    let lockfree = LockFreeCache::new();
    lockfree.populate(&hashes);

    c.bench_function("cache_mutex_lookup", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let q = &queries[i % NUM_UNIQUE_QUERIES];
            i = i.wrapping_add(1);
            black_box(mutex_cache.lookup(q));
        })
    });

    c.bench_function("cache_rwlock_lookup", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let q = &queries[i % NUM_UNIQUE_QUERIES];
            i = i.wrapping_add(1);
            black_box(rw_cache.lookup(q));
        })
    });

    c.bench_function("cache_tls_lookup", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let q = &queries[i % NUM_UNIQUE_QUERIES];
            i = i.wrapping_add(1);
            black_box(tls_lookup(q));
        })
    });

    c.bench_function("cache_lockfree_lookup", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let q = &queries[i % NUM_UNIQUE_QUERIES];
            i = i.wrapping_add(1);
            black_box(lockfree.lookup(q));
        })
    });
}

criterion_group!(benches, bench_cache);
criterion_main!(benches);
