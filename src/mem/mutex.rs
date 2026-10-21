//! Core mutexes keep native state inline, including on first use.
//!
//! The macOS standard mutex lazily boxes its pthread state. Apple's inline
//! primitive avoids that allocation: https://developer.apple.com/documentation/os/os_unfair_lock

#[cfg(target_os = "linux")]
pub(crate) use std::sync::{Mutex, MutexGuard};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("core mutexes require a supported Linux or macOS target");

#[cfg(target_os = "macos")]
pub(crate) use macos::{Mutex, MutexGuard};

#[cfg(target_os = "macos")]
mod macos {
    use core::cell::UnsafeCell;
    use core::fmt;
    use core::marker::PhantomData;
    use core::ops::{Deref, DerefMut};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{LockResult, PoisonError, TryLockError, TryLockResult};

    pub(crate) struct Mutex<T: ?Sized> {
        raw: UnsafeCell<libc::os_unfair_lock>,
        location: AtomicUsize,
        held: AtomicBool,
        poisoned: AtomicBool,
        value: UnsafeCell<T>,
    }

    // Access requires native ownership. Location validation rejects a moved
    // mutex whose guard was forgotten; waiting calls still borrow the mutex.
    unsafe impl<T: Send + ?Sized> Send for Mutex<T> {}
    unsafe impl<T: Send + ?Sized> Sync for Mutex<T> {}
    impl<T: ?Sized> std::panic::RefUnwindSafe for Mutex<T> {}
    impl<T: ?Sized> std::panic::UnwindSafe for Mutex<T> {}

    impl<T> Mutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self {
                raw: UnsafeCell::new(libc::OS_UNFAIR_LOCK_INIT),
                location: AtomicUsize::new(0),
                held: AtomicBool::new(false),
                poisoned: AtomicBool::new(false),
                value: UnsafeCell::new(value),
            }
        }
    }

    impl<T: ?Sized> Mutex<T> {
        pub(crate) fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
            self.validate_location();
            // The shared borrow keeps the native address stable while waiting
            // and until its thread-affine guard releases ownership.
            unsafe {
                libc::os_unfair_lock_lock(self.raw.get());
                self.acquired()
            }
        }

        pub(crate) fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
            self.validate_location();
            // A failed attempt neither owns nor accesses the protected value.
            unsafe {
                if libc::os_unfair_lock_trylock(self.raw.get()) {
                    self.acquired().map_err(TryLockError::Poisoned)
                } else {
                    Err(TryLockError::WouldBlock)
                }
            }
        }

        pub(crate) fn get_mut(&mut self) -> LockResult<&mut T> {
            let value = self.value.get_mut();
            if *self.poisoned.get_mut() {
                Err(PoisonError::new(value))
            } else {
                Ok(value)
            }
        }

        fn validate_location(&self) {
            const INITIALIZING: usize = 1;
            const INVALID: usize = 2;
            let location = self.raw.get().addr();
            loop {
                let prior = self.location.load(Ordering::Acquire);
                if prior == location {
                    return;
                }
                if prior == INVALID {
                    panic!("a mutex with a forgotten guard was moved");
                }
                if prior == INITIALIZING {
                    std::thread::yield_now();
                    continue;
                }
                if self
                    .location
                    .compare_exchange(prior, INITIALIZING, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    continue;
                }
                if self.held.load(Ordering::Acquire) {
                    self.location.store(INVALID, Ordering::Release);
                    panic!("a mutex with a forgotten guard was moved");
                }
                // A move requires exclusive Rust ownership, so no native
                // owner or waiter can survive at the former address. One
                // initializer resets the copied opaque state before use.
                unsafe {
                    self.raw.get().write(libc::OS_UNFAIR_LOCK_INIT);
                }
                self.location.store(location, Ordering::Release);
                return;
            }
        }

        // The current thread must already own the native lock.
        unsafe fn acquired(&self) -> LockResult<MutexGuard<'_, T>> {
            self.held.store(true, Ordering::Release);
            let guard = MutexGuard {
                mutex: self,
                entered_panicking: std::thread::panicking(),
                _thread_affine: PhantomData,
            };
            if self.poisoned.load(Ordering::Acquire) {
                Err(PoisonError::new(guard))
            } else {
                Ok(guard)
            }
        }
    }

    impl<T: Default> Default for Mutex<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T: fmt::Debug + ?Sized> fmt::Debug for Mutex<T> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            let mut output = formatter.debug_struct("Mutex");
            match self.try_lock() {
                Ok(guard) => {
                    output.field("value", &&*guard);
                }
                Err(TryLockError::Poisoned(error)) => {
                    output.field("value", &&*error.into_inner());
                }
                Err(TryLockError::WouldBlock) => {
                    output.field("value", &"locked");
                }
            }
            output
                .field("poisoned", &self.poisoned.load(Ordering::Acquire))
                .finish()
        }
    }

    #[must_use = "retain the guard while accessing the protected value"]
    pub(crate) struct MutexGuard<'lock, T: ?Sized> {
        mutex: &'lock Mutex<T>,
        entered_panicking: bool,
        _thread_affine: PhantomData<*mut ()>,
    }

    // Sharing a guard only exposes shared access to T. Its marker prevents
    // moving ownership to a different thread, as required by the native API.
    unsafe impl<T: Sync + ?Sized> Sync for MutexGuard<'_, T> {}

    impl<T: ?Sized> Deref for MutexGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // A live guard owns the native lock, including poisoned guards.
            unsafe { &*self.mutex.value.get() }
        }
    }

    impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // Native ownership and the mutable guard borrow exclude aliases.
            unsafe { &mut *self.mutex.value.get() }
        }
    }

    impl<T: fmt::Debug + ?Sized> fmt::Debug for MutexGuard<'_, T> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            (**self).fmt(formatter)
        }
    }

    impl<T: ?Sized> Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {
            if !self.entered_panicking && std::thread::panicking() {
                self.mutex.poisoned.store(true, Ordering::Release);
            }
            self.mutex.held.store(false, Ordering::Release);
            // The guard cannot leave the thread that acquired ownership.
            unsafe {
                libc::os_unfair_lock_unlock(self.mutex.raw.get());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Mutex;
    use crate::mem::guard::forbid_alloc;
    use std::sync::TryLockError;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn core_mutex_first_use_replacement_and_unlocked_moves_do_not_allocate() {
        forbid_alloc(|| {
            let mutex = Mutex::new(1);
            {
                let mut value: super::MutexGuard<'_, i32> = mutex.lock().unwrap();
                *value += 1;
                assert!(matches!(mutex.try_lock(), Err(TryLockError::WouldBlock)));
            }
            let mut moved = [mutex, Mutex::new(3)];
            *moved[0].get_mut().unwrap() += 1;
            assert_eq!(*moved[0].lock().unwrap(), 3);
            moved[1] = Mutex::new(7);
            assert_eq!(*moved[1].try_lock().unwrap(), 7);
            moved.swap(0, 1);
            assert_eq!(*moved[0].lock().unwrap(), 7);
            assert_eq!(*moved[1].lock().unwrap(), 3);
        });
    }

    #[test]
    fn core_mutex_unsized_values_keep_inline_ownership_without_allocation() {
        forbid_alloc(|| {
            let mut mutex = Mutex::new([1, 2]);
            let erased: &mut Mutex<[i32]> = &mut mutex;
            erased.get_mut().unwrap()[0] = 3;
            {
                let mut guard: super::MutexGuard<'_, [i32]> = erased.lock().unwrap();
                guard[1] = 4;
                assert_eq!(&*guard, &[3, 4]);
                assert!(matches!(erased.try_lock(), Err(TryLockError::WouldBlock)));
            }
            assert_eq!(&*erased.try_lock().unwrap(), &[3, 4]);
        });
    }

    #[test]
    fn core_mutex_concurrent_first_use_initializes_one_native_location() {
        let mutex = Mutex::new(0);
        let start = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let run = || {
                forbid_alloc(|| {
                    while !start.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    for _ in 0..32 {
                        *mutex.lock().unwrap() += 1;
                    }
                })
            };
            let first = scope.spawn(run);
            let second = scope.spawn(run);
            start.store(true, Ordering::Release);
            first.join().unwrap();
            second.join().unwrap();
        });
        forbid_alloc(|| assert_eq!(*mutex.lock().unwrap(), 64));
    }

    #[test]
    fn core_mutex_contention_blocks_and_releases_without_allocation() {
        let mutex = Mutex::new(0);
        let locked = mutex.lock().unwrap();
        let waiting = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                forbid_alloc(|| {
                    assert!(matches!(mutex.try_lock(), Err(TryLockError::WouldBlock)));
                    waiting.store(true, Ordering::Release);
                    *mutex.lock().unwrap() += 1;
                })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !waiting.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let observed = waiting.load(Ordering::Acquire);
            drop(locked);
            writer.join().unwrap();
            assert!(observed, "the competing thread did not attempt the lock");
        });
        forbid_alloc(|| assert_eq!(*mutex.lock().unwrap(), 1));
    }

    #[test]
    fn core_mutex_poisoned_guards_release_native_ownership_on_unwind() {
        let mut mutex = Mutex::new(0);
        let failed = std::panic::catch_unwind(|| {
            let mut guard = mutex.lock().unwrap();
            *guard = 9;
            panic!("exercise mutex poisoning");
        });
        assert!(failed.is_err());
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    forbid_alloc(|| {
                        let mut guard = match mutex.try_lock() {
                            Err(TryLockError::Poisoned(error)) => error.into_inner(),
                            _ => panic!("an unwound mutex did not expose its poisoned guard"),
                        };
                        assert_eq!(*guard, 9);
                        *guard += 1;
                        assert!(matches!(mutex.try_lock(), Err(TryLockError::WouldBlock)));
                    })
                })
                .join()
                .unwrap();
        });
        forbid_alloc(|| assert_eq!(*mutex.get_mut().unwrap_err().into_inner(), 10));
    }

    #[test]
    fn core_mutex_forgotten_guard_cannot_become_a_new_unlocked_mutex() {
        let mut values = [Mutex::new(0), Mutex::new(1)];
        std::mem::forget(values[0].lock().unwrap());
        values.swap(0, 1);
        #[cfg(target_os = "macos")]
        assert!(std::panic::catch_unwind(|| drop(values[1].try_lock())).is_err());
        #[cfg(target_os = "linux")]
        assert!(matches!(
            values[1].try_lock(),
            Err(TryLockError::WouldBlock)
        ));
    }
}
