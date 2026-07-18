//! Readiness reactor over kqueue (macOS / BSD).
//!
//! Level-triggered: an event fires as long as the condition holds, so the
//! server never needs to drain a socket completely in one pass — it reads
//! what fits in its fixed buffers and gets woken again for the rest. Write
//! interest is toggled on only while a send buffer has pending bytes.
//!
//! The kevent buffers are sized at construction; a `wait` never allocates.

#![cfg(any(target_os = "macos", target_os = "freebsd"))]

use std::os::fd::RawFd;
use std::time::Duration;

use crate::mem::budget::{Budget, BudgetError};

pub struct Reactor {
    kq: RawFd,
    /// Raw kevent output buffer, fixed at construction.
    raw: Box<[libc::kevent]>,
    /// Translated events, same capacity.
    events: Box<[Event]>,
    ready: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// Caller-chosen identifier registered with the fd (e.g. a connection
    /// slot index).
    pub token: u64,
    pub readable: bool,
    pub writable: bool,
    /// Peer closed its end (kqueue reports EV_EOF alongside readability).
    pub eof: bool,
}

const EMPTY_EVENT: Event = Event {
    token: 0,
    readable: false,
    writable: false,
    eof: false,
};

impl Reactor {
    pub fn new(budget: &mut Budget, max_events: usize) -> Result<Self, ReactorSetupError> {
        assert!(max_events > 0, "reactor needs a non-zero event buffer");
        budget
            .draw_array(
                max_events,
                size_of::<libc::kevent>() + size_of::<Event>(),
                "reactor_events",
            )
            .map_err(ReactorSetupError::Budget)?;
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(ReactorSetupError::Os(std::io::Error::last_os_error()));
        }
        let zero_kevent = unsafe { core::mem::zeroed::<libc::kevent>() };
        Ok(Self {
            kq,
            raw: vec![zero_kevent; max_events].into_boxed_slice(),
            events: vec![EMPTY_EVENT; max_events].into_boxed_slice(),
            ready: 0,
        })
    }

    /// Registers permanent read interest for `fd`, reported with `token`.
    pub fn register_read(&self, fd: RawFd, token: u64) -> std::io::Result<()> {
        self.change(fd, libc::EVFILT_READ, libc::EV_ADD, token)
    }

    /// Turns write-readiness reporting for `fd` on or off. Disabling when
    /// not enabled is a no-op so callers can treat this as setting the
    /// desired state rather than tracking transitions.
    pub fn set_write_interest(
        &self,
        fd: RawFd,
        token: u64,
        enabled: bool,
    ) -> std::io::Result<()> {
        let result = if enabled {
            self.change(fd, libc::EVFILT_WRITE, libc::EV_ADD, token)
        } else {
            self.change(fd, libc::EVFILT_WRITE, libc::EV_DELETE, token)
        };
        match result {
            Err(e) if !enabled && e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            other => other,
        }
    }

    /// Removes all interest for `fd`. Callers close the fd afterwards;
    /// closing also removes kqueue registrations, so ENOENT here is fine.
    pub fn deregister(&self, fd: RawFd) -> std::io::Result<()> {
        for filter in [libc::EVFILT_READ, libc::EVFILT_WRITE] {
            match self.change(fd, filter, libc::EV_DELETE, 0) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Bytes `new` will draw from the budget for a given event capacity.
    pub const fn budget_bytes(max_events: usize) -> usize {
        max_events * (size_of::<libc::kevent>() + size_of::<Event>())
    }

    /// Like [`Self::wait`], but returns only the count; events are fetched
    /// with [`Self::event`]. Lets callers mutate other state while walking
    /// the events without holding a borrow of the reactor.
    pub fn poll(&mut self, timeout: Option<Duration>) -> std::io::Result<usize> {
        self.wait(timeout).map(<[Event]>::len)
    }

    pub fn event(&self, i: usize) -> Event {
        assert!(i < self.ready, "event index out of range");
        self.events[i]
    }

    /// Blocks until at least one event or the timeout elapses; `None` waits
    /// indefinitely. Returns the ready events.
    pub fn wait(&mut self, timeout: Option<Duration>) -> std::io::Result<&[Event]> {
        let timespec = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: libc::c_long::from(d.subsec_nanos()),
        });
        let timespec_ptr = timespec
            .as_ref()
            .map_or(core::ptr::null(), |t| t as *const libc::timespec);
        let n = unsafe {
            libc::kevent(
                self.kq,
                core::ptr::null(),
                0,
                self.raw.as_mut_ptr(),
                self.raw.len() as libc::c_int,
                timespec_ptr,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                self.ready = 0;
                return Ok(&self.events[..0]);
            }
            return Err(err);
        }
        self.ready = n as usize;
        for i in 0..self.ready {
            let raw = &self.raw[i];
            self.events[i] = Event {
                token: raw.udata as u64,
                readable: raw.filter == libc::EVFILT_READ,
                writable: raw.filter == libc::EVFILT_WRITE,
                eof: raw.flags & libc::EV_EOF != 0,
            };
        }
        Ok(&self.events[..self.ready])
    }

    /// Registers a level-triggered read on `fd` that stays readable while
    /// data is pending; used for the shutdown self-pipe.
    pub fn register_read_oneshot(&self, fd: RawFd, token: u64) -> std::io::Result<()> {
        self.register_read(fd, token)
    }

    fn change(&self, fd: RawFd, filter: i16, flags: u16, token: u64) -> std::io::Result<()> {
        let change = libc::kevent {
            ident: fd as libc::uintptr_t,
            filter,
            flags,
            fflags: 0,
            data: 0,
            udata: token as *mut libc::c_void,
        };
        let rc = unsafe {
            libc::kevent(
                self.kq,
                &change,
                1,
                core::ptr::null_mut(),
                0,
                core::ptr::null(),
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        unsafe { libc::close(self.kq) };
    }
}

#[derive(Debug)]
pub enum ReactorSetupError {
    Budget(BudgetError),
    Os(std::io::Error),
}

impl std::fmt::Display for ReactorSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "reactor: {e}"),
            Self::Os(e) => write!(f, "reactor: kqueue setup failed: {e}"),
        }
    }
}

impl std::error::Error for ReactorSetupError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    fn pair() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        (a, b)
    }

    #[test]
    fn read_readiness_with_token_and_eof() {
        let mut budget = Budget::new(1 << 20);
        let mut reactor = Reactor::new(&mut budget, 16).unwrap();
        let (mut a, b) = pair();

        reactor.register_read(b.as_raw_fd(), 7).unwrap();
        assert!(reactor.wait(Some(Duration::from_millis(1))).unwrap().is_empty());

        a.write_all(b"hi").unwrap();
        let events = reactor.wait(Some(Duration::from_millis(1000))).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].token, 7);
        assert!(events[0].readable);
        assert!(!events[0].eof);

        // Level-triggered: still ready until drained.
        let events = reactor.wait(Some(Duration::from_millis(1000))).unwrap();
        assert_eq!(events.len(), 1);
        let mut buf = [0u8; 8];
        let mut b_reader = &b;
        assert_eq!(b_reader.read(&mut buf).unwrap(), 2);

        drop(a);
        let events = reactor.wait(Some(Duration::from_millis(1000))).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].eof, "peer close must surface as EOF");
    }

    #[test]
    fn write_interest_toggles() {
        let mut budget = Budget::new(1 << 20);
        let mut reactor = Reactor::new(&mut budget, 16).unwrap();
        let (a, _b) = pair();

        // Disabling before ever enabling is a no-op, not an error.
        reactor.set_write_interest(a.as_raw_fd(), 3, false).unwrap();

        reactor.set_write_interest(a.as_raw_fd(), 3, true).unwrap();
        let events = reactor.wait(Some(Duration::from_millis(1000))).unwrap();
        assert!(events.iter().any(|e| e.token == 3 && e.writable));

        reactor.set_write_interest(a.as_raw_fd(), 3, false).unwrap();
        assert!(reactor.wait(Some(Duration::from_millis(1))).unwrap().is_empty());
    }

    #[test]
    fn deregister_stops_events() {
        let mut budget = Budget::new(1 << 20);
        let mut reactor = Reactor::new(&mut budget, 16).unwrap();
        let (mut a, b) = pair();
        reactor.register_read(b.as_raw_fd(), 1).unwrap();
        a.write_all(b"x").unwrap();
        assert_eq!(reactor.wait(Some(Duration::from_millis(1000))).unwrap().len(), 1);
        reactor.deregister(b.as_raw_fd()).unwrap();
        assert!(reactor.wait(Some(Duration::from_millis(1))).unwrap().is_empty());
    }

    #[test]
    fn wait_does_not_allocate() {
        let mut budget = Budget::new(1 << 20);
        let mut reactor = Reactor::new(&mut budget, 16).unwrap();
        let (mut a, b) = pair();
        reactor.register_read(b.as_raw_fd(), 9).unwrap();
        a.write_all(b"payload").unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let events = reactor.wait(Some(Duration::from_millis(1000))).unwrap();
            assert_eq!(events.len(), 1);
        });
    }
}
