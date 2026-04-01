use super::*;

pub fn rust_pg_exception_note_query(sql: *const c_char) {
    if sql.is_null() {
        return;
    }
    unsafe {
        if *sql == 0 {
            return;
        }
        let mut ring_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_QUERY_RING_MUTEX));
        libc::snprintf(
            EXC_QUERY_RING[EXC_QUERY_RING_NEXT as usize].as_mut_ptr(),
            EXC_QUERY_MAX_LEN,
            b"%.319s\0".as_ptr() as *const c_char,
            sql,
        );
        EXC_QUERY_RING_NEXT = (EXC_QUERY_RING_NEXT + 1) % (EXC_QUERY_RING_SIZE as c_int);
        ring_guard.unlock();
    }
}

pub fn rust_pg_exception_dump_recent_queries() {
    unsafe {
        let mut ring_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_QUERY_RING_MUTEX));
        libc::fprintf(
            stderr_ptr(),
            b"[EXC_CONTEXT] Recent SQL (oldest -> newest):\n\0".as_ptr() as *const c_char,
        );
        for i in 0..EXC_QUERY_RING_SIZE {
            let idx = (EXC_QUERY_RING_NEXT + i as c_int) % (EXC_QUERY_RING_SIZE as c_int);
            let entry = EXC_QUERY_RING[idx as usize];
            if entry[0] != 0 {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] %.319s\n\0".as_ptr() as *const c_char,
                    i as c_int,
                    entry.as_ptr(),
                );
            }
        }
        libc::fflush(stderr_ptr());
        ring_guard.unlock();
    }
}

pub fn rust_pg_exception_note_phase(
    phase: *const c_char,
    sql: *const c_char,
    stmt: *const c_void,
    db: *const c_void,
) {
    unsafe {
        let mut phase_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_PHASE_RING_MUTEX));

        let slot = &mut EXC_PHASE_RING[EXC_PHASE_RING_NEXT as usize];
        libc::snprintf(
            slot.phase.as_mut_ptr(),
            slot.phase.len(),
            b"%.31s\0".as_ptr() as *const c_char,
            if phase.is_null() {
                UNKNOWN_STR.as_ptr() as *const c_char
            } else {
                phase
            },
        );
        if !sql.is_null() && *sql != 0 {
            libc::snprintf(
                slot.sql.as_mut_ptr(),
                slot.sql.len(),
                b"%.319s\0".as_ptr() as *const c_char,
                sql,
            );
        } else {
            slot.sql[0] = 0;
        }
        slot.stmt = stmt as *mut c_void;
        slot.db = db as *mut c_void;
        slot.tid = libc::pthread_self() as libc::c_ulong;

        EXC_PHASE_RING_NEXT = (EXC_PHASE_RING_NEXT + 1) % (EXC_PHASE_RING_SIZE as c_int);

        phase_guard.unlock();

        // --- seqlock: begin CRASH_LAST_QUERY write ---
        let q_seq = CRASH_LAST_QUERY_SEQ.load(Ordering::Relaxed);
        CRASH_LAST_QUERY_SEQ.store(q_seq.wrapping_add(1), Ordering::Release); // odd = writing
        let qlen = if !sql.is_null() && *sql != 0 {
            let mut wrote = libc::snprintf(
                ptr::addr_of_mut!(CRASH_LAST_QUERY) as *mut c_char,
                CRASH_QUERY_MAX_LEN,
                b"%.511s\0".as_ptr() as *const c_char,
                sql,
            );
            if wrote < 0 {
                wrote = 0;
            }
            if wrote >= CRASH_QUERY_MAX_LEN as c_int {
                wrote = CRASH_QUERY_MAX_LEN as c_int - 1;
            }
            wrote
        } else {
            CRASH_LAST_QUERY[0] = 0;
            0
        };
        CRASH_LAST_QUERY_LEN.store(qlen, Ordering::SeqCst);
        CRASH_LAST_QUERY_SEQ.store(q_seq.wrapping_add(2), Ordering::Release); // even = done
                                                                              // --- seqlock: end CRASH_LAST_QUERY write ---

        // --- seqlock: begin CRASH_LAST_PHASE write ---
        let p_seq = CRASH_LAST_PHASE_SEQ.load(Ordering::Relaxed);
        CRASH_LAST_PHASE_SEQ.store(p_seq.wrapping_add(1), Ordering::Release); // odd = writing
        let plen = if !phase.is_null() && *phase != 0 {
            let mut wrote = libc::snprintf(
                ptr::addr_of_mut!(CRASH_LAST_PHASE) as *mut c_char,
                CRASH_PHASE_MAX_LEN,
                b"%.63s\0".as_ptr() as *const c_char,
                phase,
            );
            if wrote < 0 {
                wrote = 0;
            }
            if wrote >= CRASH_PHASE_MAX_LEN as c_int {
                wrote = CRASH_PHASE_MAX_LEN as c_int - 1;
            }
            wrote
        } else {
            CRASH_LAST_PHASE[0] = 0;
            0
        };
        CRASH_LAST_PHASE_LEN.store(plen, Ordering::SeqCst);
        CRASH_LAST_PHASE_SEQ.store(p_seq.wrapping_add(2), Ordering::Release); // even = done
                                                                              // --- seqlock: end CRASH_LAST_PHASE write ---

        let trace_path = TRACE_LAST_QUERY_PATH
            .get()
            .map(|p| p.0)
            .unwrap_or(ptr::null());
        if trace_last_query_enabled() && !trace_path.is_null() && qlen > 0 {
            let fd = libc::open(
                trace_path,
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o644,
            );
            if fd >= 0 {
                if plen > 0 {
                    let _ = libc::write(
                        fd,
                        ptr::addr_of!(CRASH_LAST_PHASE) as *const c_void,
                        plen as usize,
                    );
                    let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
                }
                let _ = libc::write(
                    fd,
                    ptr::addr_of!(CRASH_LAST_QUERY) as *const c_void,
                    qlen as usize,
                );
                let _ = libc::write(fd, b"\n".as_ptr() as *const c_void, 1);
                libc::close(fd);
            }
        }
    }
}

pub fn rust_pg_exception_dump_recent_phases() {
    unsafe {
        let mut phase_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(EXC_PHASE_RING_MUTEX));

        libc::fprintf(
            stderr_ptr(),
            b"[EXC_CONTEXT] Recent phases (oldest -> newest):\n\0".as_ptr() as *const c_char,
        );
        for i in 0..EXC_PHASE_RING_SIZE {
            let idx = (EXC_PHASE_RING_NEXT + i as c_int) % (EXC_PHASE_RING_SIZE as c_int);
            let entry = &EXC_PHASE_RING[idx as usize];
            if entry.phase[0] == 0 {
                continue;
            }
            if entry.sql[0] != 0 {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] phase=%s tid=0x%lx stmt=%p db=%p sql=%.200s\n\0"
                        .as_ptr() as *const c_char,
                    i as c_int,
                    entry.phase.as_ptr(),
                    entry.tid,
                    entry.stmt,
                    entry.db,
                    entry.sql.as_ptr(),
                );
            } else {
                libc::fprintf(
                    stderr_ptr(),
                    b"[EXC_CONTEXT]   [%02d] phase=%s tid=0x%lx stmt=%p db=%p\n\0".as_ptr()
                        as *const c_char,
                    i as c_int,
                    entry.phase.as_ptr(),
                    entry.tid,
                    entry.stmt,
                    entry.db,
                );
            }
        }

        libc::fflush(stderr_ptr());
        phase_guard.unlock();
    }
}
