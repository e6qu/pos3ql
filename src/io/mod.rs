//! I/O drivers.
//!
//! The replica core is a state machine; these modules feed it readiness
//! events and move bytes. The kqueue reactor is the production driver on
//! macOS/BSD; a deterministic simulator driver joins it for whole-cluster
//! testing.

// The reactor backend is selected at compile time behind one interface:
// kqueue on macOS/BSD, epoll on Linux.
#[cfg(target_os = "linux")]
pub mod epoll;
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub mod reactor;

#[cfg(target_os = "linux")]
pub use epoll::{Event, Reactor, ReactorSetupError};
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub use reactor::{Event, Reactor, ReactorSetupError};

// Keep the `reactor` path importable on Linux too, so `crate::io::reactor`
// references resolve regardless of platform.
#[cfg(target_os = "linux")]
pub use epoll as reactor;

pub(crate) mod tcp;

#[cfg(test)]
mod tests {
    use super::Reactor;
    use crate::mem::budget::Budget;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[test]
    fn parallel_reactor_reused_descriptor_keeps_its_new_owner() {
        let mut budget = Budget::new(Reactor::budget_bytes(4));
        let mut reactor = Reactor::new(&mut budget, 4).unwrap();
        let (_old_peer, watched) = UnixStream::pair().unwrap();
        let (mut peer, replacement) = UnixStream::pair().unwrap();
        let fd = watched.as_raw_fd();
        reactor.register_read(fd, 11).unwrap();
        reactor.set_write_interest(fd, 11, true).unwrap();
        // Atomically replace the old socket so another test cannot acquire fd.
        assert_eq!(unsafe { libc::dup2(replacement.as_raw_fd(), fd) }, fd);
        drop(replacement);
        crate::mem::guard::forbid_alloc(|| {
            reactor.register_read(fd, 22).unwrap();
            reactor.deregister_owned(fd, 11).unwrap();
            assert!(reactor.wait(Some(Duration::ZERO)).unwrap().is_empty());
            peer.write_all(b"new owner").unwrap();
            let events = reactor.wait(Some(Duration::from_secs(1))).unwrap();
            assert!(!events.is_empty());
            assert!(events.iter().all(|event| event.token == 22));
            assert!(events.iter().any(|event| event.readable));
            reactor.deregister_owned(fd, 22).unwrap();
            assert!(reactor.wait(Some(Duration::ZERO)).unwrap().is_empty());
        });
    }
}
