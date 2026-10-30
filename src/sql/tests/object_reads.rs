//! HTTP fixtures share the same client-progress and deadline rules.

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub(super) fn advance(engine: &mut Engine) {
    // Simulated upload completions update fixture inventories outside the runtime guard.
    engine
        .advance_wal_publication(Instant::now(), CommitProgressMode::Inline)
        .unwrap();
    for slot in 0..engine.ckpt.as_ref().unwrap().block_read_slots() {
        if engine.pending_block_read_fd(slot).is_some() {
            crate::mem::guard::forbid_alloc(|| engine.advance_pending_block_read(slot)).unwrap();
        }
    }
}

pub(super) fn accept(engine: &mut Engine, listener: &TcpListener) -> (TcpStream, String) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
        advance(engine);
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "object GET did not connect");
                std::thread::yield_now();
            }
            Err(error) => panic!("accept object GET: {error}"),
        }
    };
    let request = read_request(engine, &mut stream);
    (stream, request)
}

pub(super) fn read_request(engine: &mut Engine, stream: &mut TcpStream) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    stream.set_nonblocking(true).unwrap();
    let mut request = [0; 8192];
    let mut used = 0;
    loop {
        advance(engine);
        match stream.read(&mut request[used..]) {
            Ok(0) => panic!("object GET ended before its headers"),
            Ok(count) => used += count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("read object GET: {error}"),
        }
        if request[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break;
        }
        assert!(
            used < request.len() && Instant::now() < deadline,
            "object GET headers did not finish"
        );
        std::thread::yield_now();
    }
    stream.set_nonblocking(false).unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    core::str::from_utf8(&request[..used]).unwrap().to_owned()
}

pub(super) fn reply(stream: &mut TcpStream, request: &str, body: &[u8]) {
    let range = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("Range").then_some(value.trim())
    });
    if let Some(range) = range {
        let (first, last) = range
            .strip_prefix("bytes=")
            .unwrap()
            .split_once('-')
            .unwrap();
        let first: usize = first.parse().unwrap();
        let last: usize = last.parse().unwrap();
        write!(stream, "HTTP/1.1 206 Partial Content\r\nETag: \"fixture\"\r\nContent-Length: {}\r\nContent-Range: bytes {first}-{last}/{}\r\nConnection: close\r\n\r\n", last-first+1, body.len()).unwrap();
        stream.write_all(&body[first..=last]).unwrap();
    } else {
        write!(stream, "HTTP/1.1 200 OK\r\nETag: \"fixture\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(body).unwrap();
    }
}

pub(super) fn replace_uncached_blocks(engine: &mut Engine, config: &Config) {
    use crate::store::{OwnedObjectStore, StackPlan};
    let blocks = engine.ckpt.as_ref().unwrap().block_stack();
    let mut budget = Budget::new(OwnedObjectStore::budget_bytes(config));
    let provider = OwnedObjectStore::new(config, &mut budget, "blocks/").unwrap();
    *blocks.lock().unwrap() = crate::store::build_tiers(
        &mut budget,
        provider,
        StackPlan::resolve(0, 0),
        std::path::Path::new(&config.data_dir),
    )
    .unwrap();
    assert_eq!(budget.remaining(), 0);
    engine.enable_async_block_reads();
}
