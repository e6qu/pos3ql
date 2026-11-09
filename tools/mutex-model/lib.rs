//! Run the actual core lock and allocation guard without TLS dependencies.

#![cfg(test)]

pub mod mem;

#[cfg(target_os = "macos")]
mod native_api;

#[global_allocator]
static ALLOCATOR: mem::guard::GuardedAllocator = mem::guard::GuardedAllocator;
