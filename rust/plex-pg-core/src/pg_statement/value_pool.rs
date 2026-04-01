use crate::sync_utils::mutex_lock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};

pub const MAX_PG_VALUES: usize = 4096;
pub const PG_VALUE_MAGIC: u32 = 0x50475641;

#[repr(C)]
pub struct PgValue {
    pub magic: u32,
    pub stmt: usize,
    pub col_idx: i32,
    pub sqlite_type: i32,
}

static PG_VALUE_IDX: AtomicU32 = AtomicU32::new(0);
static PG_VALUES: LazyLock<Vec<Mutex<PgValue>>> = LazyLock::new(|| {
    let mut v = Vec::with_capacity(MAX_PG_VALUES);
    for _ in 0..MAX_PG_VALUES {
        v.push(Mutex::new(PgValue {
            magic: 0,
            stmt: 0,
            col_idx: 0,
            sqlite_type: 0,
        }));
    }
    v
});

pub fn rust_create_column_value(stmt: usize, col_idx: i32, sqlite_type: i32) -> *mut PgValue {
    let slot = PG_VALUE_IDX.fetch_add(1, Ordering::Relaxed) as usize % MAX_PG_VALUES;
    let pool = &PG_VALUES[slot];
    let mut pv = mutex_lock(pool);
    pv.magic = PG_VALUE_MAGIC;
    pv.stmt = stmt;
    pv.col_idx = col_idx;
    pv.sqlite_type = sqlite_type;
    &mut *pv as *mut PgValue
}

pub fn rust_is_our_value(val: *const PgValue) -> i32 {
    if val.is_null() {
        return 0;
    }
    let magic = unsafe { (&*val).magic };
    i32::from(magic == PG_VALUE_MAGIC)
}
