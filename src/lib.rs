// Error values carry their message text inline (StackStr) because boxing
// them would heap-allocate, which this codebase forbids after startup.
#![allow(clippy::result_large_err)]
// Dead-code detection only works when the public surface is honest: rustc
// treats anything `pub`-reachable from the crate root as used, so a `pub` API
// that nothing outside the crate consumes silently disables the check for
// everything behind it. `unreachable_pub` keeps that surface from drifting back
// open; `dead_code` (on by default) then does its job for the rest.
#![warn(unreachable_pub)]

pub(crate) mod checkpoint;
pub mod config;
pub mod io;
pub mod mem;
pub mod pg;
pub(crate) mod prng;
pub mod s3;
pub mod server;
pub(crate) mod sim;
pub mod sql;
pub(crate) mod storage;
pub(crate) mod store;
pub(crate) mod wal;
pub(crate) mod util;
pub(crate) mod vsr;

/// All heap memory flows through the guard so that "no allocation after
/// startup" is enforced at runtime, not just by convention.
#[global_allocator]
static GLOBAL_ALLOCATOR: mem::guard::GuardedAllocator = mem::guard::GuardedAllocator;
