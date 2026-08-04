//! Readiness reactor over epoll (Linux). Mirrors the kqueue reactor's API
//! so the server is backend-agnostic.
//!
//! Level-triggered (EPOLLIN/EPOLLOUT without EPOLLET): a condition keeps
//! firing until drained, so the server reads/writes what fits its fixed
//! buffers and is woken again for the rest. Write interest is toggled by
//! rewriting the fd's interest mask. Event buffers are sized at
//! construction; a `wait` never allocates.

#![cfg(target_os = "linux")]

use std::os::fd::RawFd;
use std::time::Duration;

use crate::mem::budget::{Budget, BudgetError};

pub struct Reactor {
    epfd: RawFd,
    raw: Box<[libc::epoll_event]>,
    events: Box<[Event]>,
    interests: Box<[Interest]>,
    ready: usize,
    max_events: usize,
}

#[derive(Clone, Copy)]
struct Interest {
    fd: RawFd,
    token: u64,
    read: bool,
    write: bool,
}

const EMPTY_INTEREST: Interest = Interest {
    fd: -1,
    token: 0,
    read: false,
    write: false,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub token: u64,
    pub readable: bool,
    pub writable: bool,
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
                size_of::<libc::epoll_event>() + size_of::<Event>() + size_of::<Interest>(),
                "reactor_events",
            )
            .map_err(ReactorSetupError::Budget)?;
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(ReactorSetupError::Os(std::io::Error::last_os_error()));
        }
        let zero = libc::epoll_event { events: 0, u64: 0 };
        Ok(Self {
            epfd,
            raw: vec![zero; max_events].into_boxed_slice(),
            events: vec![EMPTY_EVENT; max_events].into_boxed_slice(),
            interests: vec![EMPTY_INTEREST; max_events].into_boxed_slice(),
            ready: 0,
            max_events,
        })
    }

    pub const fn budget_bytes(max_events: usize) -> usize {
        max_events * (size_of::<libc::epoll_event>() + size_of::<Event>() + size_of::<Interest>())
    }

    pub fn register_read(&mut self, fd: RawFd, token: u64) -> std::io::Result<()> {
        let (index, existing) = self.interest_index(fd)?;
        self.interests[index].token = token;
        self.interests[index].read = true;
        let interest = self.interests[index];
        self.ctl(
            if existing {
                libc::EPOLL_CTL_MOD
            } else {
                libc::EPOLL_CTL_ADD
            },
            interest,
        )
    }

    pub fn register_read_oneshot(&mut self, fd: RawFd, token: u64) -> std::io::Result<()> {
        self.register_read(fd, token)
    }

    pub fn set_read_interest(
        &mut self,
        fd: RawFd,
        token: u64,
        enabled: bool,
    ) -> std::io::Result<()> {
        let index = self.existing_interest_index(fd)?;
        self.interests[index].token = token;
        self.interests[index].read = enabled;
        self.ctl(libc::EPOLL_CTL_MOD, self.interests[index])
    }

    /// Level-triggered read plus optional write interest.
    pub fn set_write_interest(
        &mut self,
        fd: RawFd,
        token: u64,
        enabled: bool,
    ) -> std::io::Result<()> {
        let index = self.existing_interest_index(fd)?;
        self.interests[index].token = token;
        self.interests[index].write = enabled;
        self.ctl(libc::EPOLL_CTL_MOD, self.interests[index])
    }

    pub fn deregister(&mut self, fd: RawFd) -> std::io::Result<()> {
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        let rc = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, &mut ev) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // Closing the fd already removed it — tolerate ENOENT/EBADF.
            if !matches!(err.raw_os_error(), Some(libc::ENOENT) | Some(libc::EBADF)) {
                return Err(err);
            }
        }
        if let Some(interest) = self.interests.iter_mut().find(|interest| interest.fd == fd) {
            *interest = EMPTY_INTEREST;
        }
        Ok(())
    }

    pub fn poll(&mut self, timeout: Option<Duration>) -> std::io::Result<usize> {
        self.wait(timeout).map(<[Event]>::len)
    }

    pub fn event(&self, i: usize) -> Event {
        assert!(i < self.ready, "event index out of range");
        self.events[i]
    }

    pub fn wait(&mut self, timeout: Option<Duration>) -> std::io::Result<&[Event]> {
        let ms: libc::c_int = match timeout {
            None => -1,
            Some(d) => d.as_millis().min(i32::MAX as u128) as libc::c_int,
        };
        let n = unsafe {
            libc::epoll_wait(
                self.epfd,
                self.raw.as_mut_ptr(),
                self.max_events as libc::c_int,
                ms,
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
            let e = raw.events;
            self.events[i] = Event {
                token: raw.u64,
                readable: e & (libc::EPOLLIN as u32) != 0,
                writable: e & (libc::EPOLLOUT as u32) != 0,
                eof: e & ((libc::EPOLLHUP | libc::EPOLLRDHUP) as u32) != 0,
            };
        }
        Ok(&self.events[..self.ready])
    }

    fn interest_index(&mut self, fd: RawFd) -> std::io::Result<(usize, bool)> {
        if let Some(index) = self.interests.iter().position(|interest| interest.fd == fd) {
            return Ok((index, true));
        }
        let Some(index) = self.interests.iter().position(|interest| interest.fd == -1) else {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
        };
        self.interests[index].fd = fd;
        Ok((index, false))
    }

    fn existing_interest_index(&self, fd: RawFd) -> std::io::Result<usize> {
        self.interests
            .iter()
            .position(|interest| interest.fd == fd)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn ctl(&self, op: libc::c_int, interest: Interest) -> std::io::Result<()> {
        let mut mask = 0u32;
        if interest.read {
            mask |= libc::EPOLLIN as u32;
        }
        if interest.write {
            mask |= libc::EPOLLOUT as u32;
        }
        // Watch for peer half-close alongside read interest.
        let mut ev = libc::epoll_event {
            events: mask | (libc::EPOLLRDHUP as u32),
            u64: interest.token,
        };
        let rc = unsafe { libc::epoll_ctl(self.epfd, op, interest.fd, &mut ev) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        unsafe { libc::close(self.epfd) };
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
            Self::Os(e) => write!(f, "reactor: epoll setup failed: {e}"),
        }
    }
}

impl std::error::Error for ReactorSetupError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn write_interest_preserves_disabled_read_interest() {
        let mut budget = Budget::new(1 << 20);
        let mut reactor = Reactor::new(&mut budget, 4).unwrap();
        let (mut peer, watched) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        watched.set_nonblocking(true).unwrap();

        reactor.register_read(watched.as_raw_fd(), 9).unwrap();
        reactor
            .set_read_interest(watched.as_raw_fd(), 9, false)
            .unwrap();
        peer.write_all(b"parked request").unwrap();
        reactor
            .set_write_interest(watched.as_raw_fd(), 9, true)
            .unwrap();

        let events = reactor.wait(Some(Duration::from_millis(1000))).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.token == 9 && event.writable)
        );
        assert!(
            !events
                .iter()
                .any(|event| event.token == 9 && event.readable)
        );
    }
}
