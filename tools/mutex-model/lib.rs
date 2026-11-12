//! Run the actual core lock and allocation guard without TLS dependencies.

#![cfg(test)]

pub mod mem;
// The model compiles shared runtime files whose other users live outside it.
#[allow(dead_code)]
#[path = "../../src/test_wait.rs"]
mod test_wait;

#[cfg(target_os = "macos")]
mod native_api;

#[global_allocator]
static ALLOCATOR: mem::guard::GuardedAllocator = mem::guard::GuardedAllocator;
