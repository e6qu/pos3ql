//! Hang detectors for tests that drive fake peers and background threads.
//!
//! A loaded test host can stall any thread for seconds, so these bounds only
//! catch a fixture that will never progress; they never encode an expected
//! latency. Tests that check a timeout use their own explicit duration.

use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Longest a fixture waits for a peer or thread before reporting a hang.
pub(crate) const HANG: Duration = Duration::from_secs(60);

/// Deadline for one fixture wait that starts now.
pub(crate) fn deadline() -> Instant {
    Instant::now() + HANG
}

/// Reads an HTTP request head (and any bytes after it) into `buffer` for a
/// background fake server. Returns `None` when `stop` is set first, so the
/// server can exit when a client connects but its test finishes without
/// sending, and when the client closes the connection before sending (a
/// statement that restarts abandons its read); otherwise returns the bytes
/// read and the head length.
pub(crate) fn read_request_head(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    stop: &AtomicBool,
) -> Option<(usize, usize)> {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();
    let deadline = deadline();
    let mut received = 0;
    let head = loop {
        if let Some(at) = buffer[..received]
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
        {
            break at + 4;
        }
        assert!(
            received < buffer.len(),
            "fixture request headers are too large"
        );
        match stream.read(&mut buffer[received..]) {
            Ok(0) if received == 0 => return None,
            Ok(0) => panic!("request ended before its headers"),
            Ok(count) => received += count,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if stop.load(Ordering::Acquire) {
                    return None;
                }
                assert!(Instant::now() < deadline, "request headers did not arrive");
            }
            Err(error) => panic!("read request headers: {error}"),
        }
    };
    stream.set_read_timeout(Some(HANG)).unwrap();
    stream.set_write_timeout(Some(HANG)).unwrap();
    Some((received, head))
}
