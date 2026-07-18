//! I/O drivers.
//!
//! The replica core is a state machine; these modules feed it readiness
//! events and move bytes. The kqueue reactor is the production driver on
//! macOS/BSD; a deterministic simulator driver joins it for whole-cluster
//! testing.

// The reactor backend is selected at compile time behind one interface:
// kqueue on macOS/BSD, epoll on Linux.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub mod reactor;
#[cfg(target_os = "linux")]
pub mod epoll;

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub use reactor::{Event, Reactor, ReactorSetupError};
#[cfg(target_os = "linux")]
pub use epoll::{Event, Reactor, ReactorSetupError};

// Keep the `reactor` path importable on Linux too, so `crate::io::reactor`
// references resolve regardless of platform.
#[cfg(target_os = "linux")]
pub use epoll as reactor;
