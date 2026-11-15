//! HTTP fixtures share the same client-progress and deadline rules.

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

pub(crate) use crate::sql::HttpObjectServer;

#[test]
fn http_object_fixture_preserves_missing_keys_conditions_ranges_and_listing() {
    use crate::object_store::{ByteRange, Client, EntityTag, Precondition};

    let mut config = test_config("http-object-contract");
    config.object_store_on = true;
    config.object_store_bucket = format!("http-object-contract-{}", std::process::id());
    config.object_store_prefix = "contract/root/".to_owned();
    let server = HttpObjectServer::start(&mut config);
    let mut budget = Budget::new(Client::budget_bytes(&config));
    let mut client = Client::new(&config, &mut budget).unwrap();
    let missing =
        crate::mem::guard::forbid_alloc(|| client.get("restore-pending", None)).unwrap_err();
    assert!(missing.is_not_found(), "{missing}");
    assert_eq!(
        crate::mem::guard::forbid_alloc(|| client.list("commits/", |_| {})).unwrap(),
        0
    );

    let first = crate::mem::guard::forbid_alloc(|| {
        client.put("fixture/a &<", b"alpha", Precondition::IfNoneMatchAny)
    })
    .unwrap();
    let exists = crate::mem::guard::forbid_alloc(|| {
        client.put("fixture/a &<", b"replacement", Precondition::IfNoneMatchAny)
    })
    .unwrap_err();
    assert!(exists.is_precondition_failed(), "{exists}");
    let stale = EntityTag::parse("\"stale\"").unwrap();
    let conflict = crate::mem::guard::forbid_alloc(|| {
        client.put(
            "fixture/a &<",
            b"replacement",
            Precondition::IfMatch(&stale),
        )
    })
    .unwrap_err();
    assert!(conflict.is_precondition_failed(), "{conflict}");
    let metadata = crate::mem::guard::forbid_alloc(|| {
        client.get("fixture/a &<", Some(ByteRange::new(1, 3).unwrap()))
    })
    .unwrap();
    assert_eq!(metadata.etag, first);
    assert_eq!(client.body_bytes(), b"lph");
    let metadata = crate::mem::guard::forbid_alloc(|| {
        client.get("fixture/a &<", Some(ByteRange::new(1, 1024).unwrap()))
    })
    .unwrap();
    assert_eq!(metadata.etag, first);
    assert_eq!(client.body_bytes(), b"lpha");
    let range_error = crate::mem::guard::forbid_alloc(|| {
        client.get("fixture/a &<", Some(ByteRange::new(5, 5).unwrap()))
    })
    .unwrap_err();
    assert!(matches!(
        range_error,
        crate::object_store::Error::Status { code: 416, .. }
    ));
    let replaced = crate::mem::guard::forbid_alloc(|| {
        client.put("fixture/a &<", b"updated", Precondition::IfMatch(&first))
    })
    .unwrap();
    assert_ne!(replaced, first);
    let metadata = crate::mem::guard::forbid_alloc(|| client.get("fixture/a &<", None)).unwrap();
    assert_eq!(metadata.etag, replaced);
    assert_eq!(client.body_bytes(), b"updated");
    crate::mem::guard::forbid_alloc(|| {
        client.put("fixture/b", b"second", Precondition::IfNoneMatchAny)
    })
    .unwrap();

    let mut keys = [crate::util::StackStr::<256>::new(); 2];
    let mut count = 0usize;
    let listed = crate::mem::guard::forbid_alloc(|| {
        client.list_batch("fixture/", "", 1, |key| {
            keys[count] = crate::util::StackStr::from_str(key);
            count += 1;
        })
    })
    .unwrap();
    assert_eq!(listed, 1);
    assert_eq!(keys[0].as_str(), "fixture/a &<");
    count = 0;
    let listed = crate::mem::guard::forbid_alloc(|| {
        client.list_batch("fixture/", "fixture/a &<", 1, |key| {
            keys[count] = crate::util::StackStr::from_str(key);
            count += 1;
        })
    })
    .unwrap();
    assert_eq!(listed, 1);
    assert_eq!(keys[0].as_str(), "fixture/b");
    crate::mem::guard::forbid_alloc(|| client.delete("fixture/a &<")).unwrap();
    let missing = crate::mem::guard::forbid_alloc(|| client.get("fixture/a &<", None)).unwrap_err();
    assert!(missing.is_not_found(), "{missing}");
    crate::mem::guard::forbid_alloc(|| client.delete("fixture/a &<")).unwrap();
    let mut engine_budget = test_engine_budget(&config, 1 << 29);
    let engine = Engine::new(&config, &mut engine_budget).unwrap();
    assert!(engine.storage.spill_attached());
    drop(engine);
    drop((client, server));
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

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
    let deadline = crate::test_wait::deadline();
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
    let deadline = crate::test_wait::deadline();
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
        .set_write_timeout(Some(crate::test_wait::HANG))
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
    replace_blocks(engine, config, 0);
}

/// Replaces the engine's block stack with one reading `config`'s object
/// endpoint through a RAM cache of `cache_bytes` and no disk tier.
pub(super) fn replace_blocks(engine: &mut Engine, config: &Config, cache_bytes: usize) {
    use crate::store::{OwnedObjectStore, StackPlan};
    let blocks = engine.ckpt.as_ref().unwrap().block_stack();
    let plan = StackPlan::resolve(cache_bytes, 0);
    let mut budget = Budget::new(OwnedObjectStore::budget_bytes(config) + plan.ram_budget_bytes());
    let provider = OwnedObjectStore::new(config, &mut budget, "blocks/").unwrap();
    *blocks.lock().unwrap() = crate::store::build_tiers(
        &mut budget,
        provider,
        plan,
        std::path::Path::new(&config.data_dir),
    )
    .unwrap();
    assert_eq!(budget.remaining(), 0);
    engine.enable_async_block_reads();
}
