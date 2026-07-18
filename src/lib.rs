// Error values carry their message text inline (StackStr) because boxing
// them would heap-allocate, which this codebase forbids after startup.
#![allow(clippy::result_large_err)]

pub mod checkpoint;
pub mod config;
pub mod io;
pub mod mem;
pub mod pg;
pub mod prng;
pub mod s3;
pub mod server;
pub mod sim;
pub mod sql;
pub mod storage;
pub mod wal;
pub mod util;
pub mod vsr;

/// All heap memory flows through the guard so that "no allocation after
/// startup" is enforced at runtime, not just by convention.
#[global_allocator]
static GLOBAL_ALLOCATOR: mem::guard::GuardedAllocator = mem::guard::GuardedAllocator;
