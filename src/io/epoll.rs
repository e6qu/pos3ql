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
    ready: usize,
    /// Remembered read-interest per fd is implicit; we track only whether
    /// write interest is on to build the correct mask on toggle.
    max_events: usize,
}

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
                size_of::<libc::epoll_event>() + size_of::<Event>(),
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
            ready: 0,
            max_events,
        })
    }

    pub const fn budget_bytes(max_events: usize) -> usize {
        max_events * (size_of::<libc::epoll_event>() + size_of::<Event>())
    }

    pub fn register_read(&self, fd: RawFd, token: u64) -> std::io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, token, libc::EPOLLIN as u32)
    }

    pub fn register_read_oneshot(&self, fd: RawFd, token: u64) -> std::io::Result<()> {
        self.register_read(fd, token)
    }

    /// Level-triggered read plus optional write interest.
    pub fn set_write_interest(
        &self,
        fd: RawFd,
        token: u64,
        enabled: bool,
    ) -> std::io::Result<()> {
        let mut mask = libc::EPOLLIN as u32;
        if enabled {
            mask |= libc::EPOLLOUT as u32;
        }
        self.ctl(libc::EPOLL_CTL_MOD, fd, token, mask)
    }

    pub fn deregister(&self, fd: RawFd) -> std::io::Result<()> {
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        let rc = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, &mut ev) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // Closing the fd already removed it — tolerate ENOENT/EBADF.
            if matches!(err.raw_os_error(), Some(libc::ENOENT) | Some(libc::EBADF)) {
                return Ok(());
            }
            return Err(err);
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

    fn ctl(&self, op: libc::c_int, fd: RawFd, token: u64, mask: u32) -> std::io::Result<()> {
        // Watch for peer half-close alongside read interest.
        let mut ev = libc::epoll_event {
            events: mask | (libc::EPOLLRDHUP as u32),
            u64: token,
        };
        let rc = unsafe { libc::epoll_ctl(self.epfd, op, fd, &mut ev) };
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
