//! Run the actual core lock and allocation guard without TLS dependencies.

#![cfg(test)]

pub mod mem;

#[global_allocator]
static ALLOCATOR: mem::guard::GuardedAllocator = mem::guard::GuardedAllocator;
