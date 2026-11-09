//! Direct Apple API control for the core mutex ownership tests.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

struct NativeLock {
    raw: UnsafeCell<libc::os_unfair_lock>,
    value: UnsafeCell<usize>,
}

// The scoped threads keep the address stable. Only a native lock owner reads
// or writes the value until both threads have joined.
unsafe impl Sync for NativeLock {}

impl NativeLock {
    fn increment(&self) {
        // This control uses no pos3ql mutex or location metadata.
        unsafe {
            libc::os_unfair_lock_lock(self.raw.get());
            *self.value.get() += 1;
            libc::os_unfair_lock_unlock(self.raw.get());
        }
    }
}

#[test]
fn core_mutex_direct_native_api_repeated_empty_queue() {
    let lock = NativeLock {
        raw: UnsafeCell::new(libc::OS_UNFAIR_LOCK_INIT),
        value: UnsafeCell::new(0),
    };
    let lock = &lock;
    let start = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let run = || {
            crate::mem::guard::forbid_alloc(|| {
                while !start.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                for _ in 0..32 {
                    lock.increment();
                }
            });
        };
        let first = scope.spawn(run);
        let second = scope.spawn(run);
        start.store(true, Ordering::Release);
        first.join().unwrap();
        second.join().unwrap();
    });
    // Both owners have finished and released the native lock.
    assert_eq!(unsafe { *lock.value.get() }, 64);
}
