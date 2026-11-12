//! HTTP fixtures share the same client-progress and deadline rules.

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

/// Real HTTP clients run under the allocation guard; provider state lives on
/// the server thread, matching an external object-store service.
pub(crate) struct HttpObjectServer {
    stop: Arc<AtomicBool>,
    pub(super) requests: Arc<AtomicUsize>,
    /// Every distinct key an object GET read.
    pub(super) fetched: Arc<crate::mem::mutex::Mutex<std::collections::HashSet<String>>>,
    pub(super) uploads: Arc<AtomicUsize>,
    pub(super) hold_response: Arc<AtomicBool>,
    hold_upload_response: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub(crate) struct HeldUploadResponses<'a> {
    held: &'a AtomicBool,
}

impl Drop for HeldUploadResponses<'_> {
    fn drop(&mut self) {
        self.held.store(false, Ordering::Release);
    }
}

impl HttpObjectServer {
    pub(crate) fn hold_upload_responses(&self) -> HeldUploadResponses<'_> {
        assert!(!self.hold_upload_response.swap(true, Ordering::AcqRel));
        HeldUploadResponses {
            held: &self.hold_upload_response,
        }
    }

    pub(crate) fn start(config: &mut Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        config.object_store_sim = false;
        config.object_store_endpoint = listener.local_addr().unwrap().to_string();
        config.object_store_access_key = "test-access".to_string();
        config.object_store_secret_key = "test-secret".to_string();
        let prefix = format!("/{}/", config.object_store_bucket);
        let mut fixture_config = config.clone();
        fixture_config.object_store_sim = true;
        // HTTP paths already contain the configured object prefix.
        fixture_config.object_store_prefix.clear();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let fetched = Arc::new(crate::mem::mutex::Mutex::new(
            std::collections::HashSet::new(),
        ));
        let uploads = Arc::new(AtomicUsize::new(0));
        let hold_response = Arc::new(AtomicBool::new(false));
        let hold_upload_response = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server_requests = Arc::clone(&requests);
        let server_fetched = Arc::clone(&fetched);
        let server_uploads = Arc::clone(&uploads);
        let server_hold_response = Arc::clone(&hold_response);
        let server_hold_upload_response = Arc::clone(&hold_upload_response);
        let server = std::thread::spawn(move || {
            let mut fixture_budget =
                Budget::new(crate::object_store::Client::budget_bytes(&fixture_config));
            let backing = Arc::new(crate::mem::mutex::Mutex::new(
                crate::object_store::Client::new(&fixture_config, &mut fixture_budget).unwrap(),
            ));
            // Each connection is served on its own thread, as a real service
            // serves them concurrently: a synchronous upload must not wait
            // behind a reactor-owned GET whose request is not yet written.
            let mut connections = Vec::new();
            while !server_stop.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("accept object read: {error}"),
                };
                let backing = Arc::clone(&backing);
                let prefix = prefix.clone();
                let server_stop = Arc::clone(&server_stop);
                let server_requests = Arc::clone(&server_requests);
                let server_fetched = Arc::clone(&server_fetched);
                let server_uploads = Arc::clone(&server_uploads);
                let server_hold_response = Arc::clone(&server_hold_response);
                let server_hold_upload_response = Arc::clone(&server_hold_upload_response);
                connections.push(std::thread::spawn(move || {
                let mut request = [0u8; 8192];
                let Some((received, header_end)) =
                    crate::test_wait::read_request_head(&mut stream, &mut request, &server_stop)
                else {
                    return;
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let mut words = headers.lines().next().unwrap().split_whitespace();
                let method = words.next().unwrap();
                let target = words.next().unwrap();
                let (path, query) = target
                    .split_once('?')
                    .map_or((target, None), |(path, query)| (path, Some(query)));
                let key = path.strip_prefix(&prefix);
                let mut content_length = None;
                let mut range = None;
                let mut if_none_match = None;
                let mut if_match = None;
                for line in headers.lines().skip(1).filter(|line| !line.is_empty()) {
                    let (name, value) = line.split_once(':').expect("HTTP header field");
                    let value = value.trim();
                    if name.eq_ignore_ascii_case("Content-Length") {
                        content_length = Some(value.parse::<usize>().unwrap());
                    } else if name.eq_ignore_ascii_case("Range") {
                        range = Some(value);
                    } else if name.eq_ignore_ascii_case("If-None-Match") {
                        if_none_match = Some(value);
                    } else if name.eq_ignore_ascii_case("If-Match") {
                        if_match = Some(crate::object_store::EntityTag::parse(value).unwrap());
                    }
                }
                let length = match content_length {
                    Some(length) => length,
                    None => {
                        assert_ne!(method, "PUT", "PUT requires Content-Length");
                        0
                    }
                };
                assert!(
                    length <= backing.lock().unwrap().response_capacity(),
                    "fixture request body is too large"
                );
                let mut body = vec![0u8; length];
                let buffered = received - header_end;
                assert!(
                    buffered <= length,
                    "unexpected bytes after the request body"
                );
                body[..buffered].copy_from_slice(&request[header_end..received]);
                stream.read_exact(&mut body[buffered..]).unwrap();
                if let Some(query) = query {
                    assert_eq!(method, "GET");
                    assert_eq!(path, prefix.trim_end_matches('/'));
                    reply_object_list(&mut backing.lock().unwrap(), &mut stream, query);
                    return;
                }
                let key = decode_http_component(key.expect("fixture object path"));
                let key = key.as_str();
                match method {
                    "PUT" => {
                        let condition = match (if_none_match, if_match.as_ref()) {
                            (Some("*"), None) => crate::object_store::Precondition::IfNoneMatchAny,
                            (None, Some(tag)) => crate::object_store::Precondition::IfMatch(tag),
                            (None, None) => crate::object_store::Precondition::None,
                            _ => panic!("unsupported conditional request"),
                        };
                        let stored = backing.lock().unwrap().put(key, &body, condition);
                        match stored {
                            Ok(tag) => {
                                server_uploads.fetch_add(1, Ordering::Relaxed);
                                while server_hold_upload_response.load(Ordering::Acquire)
                                    && !server_stop.load(Ordering::Acquire)
                                {
                                    std::thread::sleep(Duration::from_millis(1));
                                }
                                if server_stop.load(Ordering::Acquire) {
                                    return;
                                }
                                write!(stream, "HTTP/1.1 200 OK\r\nETag: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", tag.as_str()).unwrap();
                            }
                            Err(crate::object_store::Error::Status { code: 412, .. }) => {
                                reply_object_error(
                                    &mut stream,
                                    412,
                                    "PreconditionFailed",
                                    "precondition failed",
                                );
                            }
                            Err(error) => panic!("fixture object upload failed: {error}"),
                        }
                    }
                    "DELETE" => {
                        backing.lock().unwrap().delete(key).unwrap();
                        stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    }
                    "GET" => {
                        let mut backing = backing.lock().unwrap();
                        let metadata = match backing.get(key, None) {
                            Ok(metadata) => metadata,
                            Err(error) if error.is_not_found() => {
                                reply_object_error(&mut stream, 404, "NoSuchKey", "object missing");
                                return;
                            }
                            Err(error) => panic!("fixture object read failed: {error}"),
                        };
                        let data = backing.body_bytes().to_vec();
                        drop(backing);
                        let data = data.as_slice();
                        server_requests.fetch_add(1, Ordering::Relaxed);
                        server_fetched.lock().unwrap().insert(key.to_owned());
                        while server_hold_response.load(Ordering::Acquire)
                            && !server_stop.load(Ordering::Acquire)
                        {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        if server_stop.load(Ordering::Acquire) {
                            return;
                        }
                        if let Some(range) = range {
                            let (first, last) = range
                                .strip_prefix("bytes=")
                                .unwrap()
                                .split_once('-')
                                .unwrap();
                            let first: usize = first.parse().unwrap();
                            let last: usize = last.parse().unwrap();
                            if first >= data.len() || last < first {
                                write!(stream, "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", data.len()).unwrap();
                                return;
                            }
                            let last = last.min(data.len() - 1);
                            let selected = &data[first..=last];
                            write!(stream, "HTTP/1.1 206 Partial Content\r\nETag: {}\r\nContent-Length: {}\r\nContent-Range: bytes {first}-{last}/{}\r\nConnection: close\r\n\r\n", metadata.etag.as_str(), selected.len(), data.len()).unwrap();
                            stream.write_all(selected).unwrap();
                        } else {
                            write!(stream, "HTTP/1.1 200 OK\r\nETag: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", metadata.etag.as_str(), data.len()).unwrap();
                            stream.write_all(data).unwrap();
                        }
                    }
                    _ => panic!("unsupported fixture method: {method}"),
                }
                }));
            }
            for connection in connections {
                connection
                    .join()
                    .expect("HTTP object fixture connection failed");
            }
        });
        Self {
            stop,
            requests,
            fetched,
            uploads,
            hold_response,
            hold_upload_response,
            thread: Some(server),
        }
    }
}

impl Drop for HttpObjectServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.expect("HTTP object fixture thread failed");
            }
        }
    }
}

fn decode_http_component(input: &str) -> String {
    let mut output = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        output.push(if byte == b'%' {
            let high = char::from(bytes.next().expect("URI escape high digit"))
                .to_digit(16)
                .expect("URI hex digit");
            let low = char::from(bytes.next().expect("URI escape low digit"))
                .to_digit(16)
                .expect("URI hex digit");
            ((high << 4) | low) as u8
        } else {
            byte
        });
    }
    String::from_utf8(output).expect("fixture URI must be UTF-8")
}

fn reply_object_error(stream: &mut TcpStream, status: u16, code: &str, message: &str) {
    let reason = match status {
        404 => "Not Found",
        412 => "Precondition Failed",
        _ => panic!("unsupported fixture error status: {status}"),
    };
    let body = format!("<Error><Code>{code}</Code><Message>{message}</Message></Error>");
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
}

fn reply_object_list(
    backing: &mut crate::object_store::Client,
    stream: &mut TcpStream,
    query: &str,
) {
    let mut prefix = None;
    let mut after = String::new();
    let mut limit = 1000usize;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').expect("fixture list query field");
        let value = decode_http_component(value);
        match name {
            "encoding-type" => assert_eq!(value, "url"),
            "list-type" => assert_eq!(value, "2"),
            "prefix" => {
                assert!(prefix.is_none());
                prefix = Some(value);
            }
            "start-after" => after = value,
            "continuation-token" => after = decode_http_component(&value),
            "max-keys" => limit = value.parse().expect("fixture list maximum keys"),
            _ => panic!("unsupported fixture list field: {name}"),
        }
    }
    assert!((1..=1000).contains(&limit));
    let prefix = prefix.expect("fixture list prefix");
    let mut keys = Vec::new();
    backing
        .list_batch(&prefix, &after, limit + 1, |key| keys.push(key.to_owned()))
        .unwrap();
    let truncated = keys.len() > limit;
    keys.truncate(limit);
    let mut body = format!(
        "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>{truncated}</IsTruncated>"
    );
    let mut last = String::new();
    for key in &keys {
        last.clear();
        crate::object_store::signature_v4::uri_encode(&mut last, key, false).unwrap();
        body.push_str("<Contents><Key>");
        body.push_str(&last);
        body.push_str("</Key></Contents>");
    }
    if truncated {
        body.push_str("<NextContinuationToken>");
        body.push_str(&last);
        body.push_str("</NextContinuationToken>");
    }
    body.push_str("</ListBucketResult>");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body.as_bytes()).unwrap();
}

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
