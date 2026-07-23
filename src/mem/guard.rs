//! Guarding global allocator.
//!
//! Passes through to the system allocator, but treats any allocation as a
//! fault once the process has been [`freeze`]-d (startup complete) or while
//! the current thread is inside a [`forbid_alloc`] scope. Deallocation is
//! always allowed: freeing cannot grow the footprint, and forbidding it
//! would fire on unavoidable teardown paths such as thread-local
//! destruction.
//!
//! A fault writes a message straight to stderr with `libc::write` and
//! aborts — the normal panic machinery allocates, which would recurse.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::alloc::{GlobalAlloc, Layout, System};

pub struct GuardedAllocator;

static FROZEN: AtomicBool = AtomicBool::new(false);

/// Test-only escape hatch: when set, a fault increments [`violations`]
/// instead of aborting, and the allocation proceeds. Nothing in the server
/// enables this; it exists so the guard itself is testable.
static COUNT_ONLY: AtomicBool = AtomicBool::new(false);
static VIOLATIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    // const-initialized and droppable-free: accessing it never allocates,
    // which matters because it is read on every allocation.
    static FORBIDDEN: Cell<bool> = const { Cell::new(false) };
}

/// Marks startup as complete. Every subsequent heap allocation in any
/// thread is a fault. Irreversible.
pub fn freeze() {
    FROZEN.store(true, Ordering::SeqCst);
}

/// Runs `f` with heap allocation treated as a fault on this thread.
/// Used by tests to prove hot paths allocate nothing.
pub fn forbid_alloc<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            FORBIDDEN.with(|c| c.set(self.0));
        }
    }
    let prev = FORBIDDEN.with(|c| c.replace(true));
    let _restore = Restore(prev);
    f()
}

pub fn set_count_only(enabled: bool) {
    COUNT_ONLY.store(enabled, Ordering::SeqCst);
}

pub fn violations() -> u64 {
    VIOLATIONS.load(Ordering::SeqCst)
}

#[cold]
#[inline(never)]
fn fault() {
    if COUNT_ONLY.load(Ordering::SeqCst) {
        VIOLATIONS.fetch_add(1, Ordering::SeqCst);
        return;
    }
    // A panic already in flight is itself the loud failure; the unwind
    // machinery allocates its payload, and aborting here would mask the
    // original message.
    if std::thread::panicking() {
        return;
    }
    let msg: &[u8] = b"pos3ql: heap allocation after freeze or inside forbid_alloc scope; aborting\n";
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
        // An alloc-free backtrace: `backtrace_symbols_fd` writes straight to
        // the fd without malloc, so the guard can say *where* without
        // recursing into itself.
        let mut frames = [core::ptr::null_mut::<libc::c_void>(); 64];
        let n = libc::backtrace(frames.as_mut_ptr(), 64);
        libc::backtrace_symbols_fd(frames.as_ptr(), n, 2);
    }
    std::process::abort();
}

#[inline]
fn check() {
    if FROZEN.load(Ordering::Relaxed) || FORBIDDEN.with(|c| c.get()) {
        fault();
    }
}

unsafe impl GlobalAlloc for GuardedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        check();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test covers all guard behavior: COUNT_ONLY is process-global,
    // and splitting this across #[test] functions would let the parallel
    // test harness interleave with it.
    #[test]
    fn forbid_alloc_detects_allocation() {
        set_count_only(true);

        let before = violations();
        forbid_alloc(|| {
            let v: Vec<u8> = Vec::with_capacity(32);
            drop(v);
        });
        let during = violations();
        assert!(
            during > before,
            "allocation inside forbid_alloc must be detected"
        );

        // Outside the scope allocation is clean again.
        let v: Vec<u8> = Vec::with_capacity(32);
        drop(v);
        assert_eq!(violations(), during);

        // The scope restores the previous state even on unwind.
        let unwound = std::panic::catch_unwind(|| {
            forbid_alloc(|| panic!("boom"));
        });
        assert!(unwound.is_err());
        let after_unwind = violations();
        let v: Vec<u8> = Vec::with_capacity(32);
        drop(v);
        assert_eq!(violations(), after_unwind);

        set_count_only(false);
    }
}
