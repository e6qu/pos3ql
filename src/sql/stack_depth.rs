//! PostgreSQL's `check_stack_depth` for pos3ql's recursive evaluation: a
//! recursion that nests one level per input level (JSON containers) asks,
//! at each level, whether the thread's fixed stack keeps a margin, and
//! fails with PostgreSQL's 54001 otherwise, instead of overflowing.

use core::cell::Cell;

use crate::sql::eval::SqlError;

/// Stack kept free below the deepest checked frame: room for the frames a
/// level runs between two checks and for reporting the error, in an
/// unoptimized build.
const MARGIN_BYTES: usize = 8 << 20;

thread_local! {
    /// The lowest usable address of this thread's stack, once looked up.
    static STACK_LOW: Cell<usize> = const { Cell::new(0) };
}

/// The lowest address of the calling thread's stack.
fn stack_low() -> usize {
    let cached = STACK_LOW.with(Cell::get);
    if cached != 0 {
        return cached;
    }
    let low = thread_stack_low();
    STACK_LOW.with(|cell| cell.set(low));
    low
}

#[cfg(target_os = "macos")]
fn thread_stack_low() -> usize {
    // SAFETY: both calls only read the calling thread's attributes.
    unsafe {
        let thread = libc::pthread_self();
        let top = libc::pthread_get_stackaddr_np(thread) as usize;
        top - libc::pthread_get_stacksize_np(thread)
    }
}

#[cfg(not(target_os = "macos"))]
fn thread_stack_low() -> usize {
    // SAFETY: the attribute object is initialized by `pthread_getattr_np`
    // and destroyed before returning; the calls read the calling thread's
    // stack bounds only.
    unsafe {
        let mut attributes: libc::pthread_attr_t = core::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attributes) != 0 {
            return 0;
        }
        let mut address: *mut libc::c_void = core::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let found = libc::pthread_attr_getstack(&attributes, &mut address, &mut size);
        libc::pthread_attr_destroy(&mut attributes);
        if found != 0 { 0 } else { address as usize }
    }
}

/// PostgreSQL's error when an evaluation nests past its `max_stack_depth`
/// (2048kB by default).
pub(crate) fn stack_depth_exceeded() -> SqlError {
    crate::sql::error_position::with_hint(
        crate::sql_err!(
            crate::sql::eval::sqlstate::STATEMENT_TOO_COMPLEX,
            "stack depth limit exceeded"
        ),
        "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.",
    )
}

/// Fails with [`stack_depth_exceeded`] when the calling frame leaves less
/// than the margin of the thread's stack.
#[inline(never)]
pub(crate) fn check_stack_depth() -> Result<(), SqlError> {
    let marker = 0u8;
    let here = core::ptr::from_ref(&marker) as usize;
    let low = stack_low();
    if low != 0 && here < low.saturating_add(MARGIN_BYTES) {
        return Err(stack_depth_exceeded());
    }
    Ok(())
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    #[test]
    fn a_deep_recursion_fails_before_the_stack_overflows() {
        fn descend(depth: usize) -> Result<usize, SqlError> {
            check_stack_depth()?;
            let pad = core::hint::black_box([0u8; 512]);
            descend(depth + 1).map(|reached| reached + usize::from(pad[0]))
        }
        // The test thread's fixed stack has room for thousands of levels,
        // but not unbounded ones.
        let error = std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| descend(0).unwrap_err())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(error.message.as_str(), "stack depth limit exceeded");
        check_stack_depth().unwrap();
    }
}
