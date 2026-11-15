//! A real HTTP object-store fixture for native tests: provider state lives on
//! a server thread, matching an external object-store service. Both library
//! test suites start it.

use crate::config::Config;
use crate::mem::budget::Budget;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

/// Real HTTP clients run under the allocation guard; provider state lives on
/// the server thread, matching an external object-store service.
pub(crate) struct HttpObjectServer {
    stop: Arc<AtomicBool>,
    pub(crate) requests: Arc<AtomicUsize>,
    /// Every distinct key an object GET read.
    pub(crate) fetched: Arc<crate::mem::mutex::Mutex<std::collections::HashSet<String>>>,
    pub(crate) uploads: Arc<AtomicUsize>,
    pub(crate) hold_response: Arc<AtomicBool>,
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
            let mut backing =
                crate::object_store::Client::new(&fixture_config, &mut fixture_budget).unwrap();
            // Requests are served in the order their heads arrive: the engine
            // may open a read and finish a synchronous one before it sends
            // the first read's request.
            let mut waiting: Vec<(std::net::TcpStream, Box<[u8; 8192]>, usize)> = Vec::new();
            while !server_stop.load(Ordering::Acquire) {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(true).unwrap();
                            waiting.push((stream, Box::new([0u8; 8192]), 0));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("accept object read: {error}"),
                    }
                }
                let head_end = |request: &[u8]| {
                    request
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                        .map(|at| at + 4)
                };
                let mut ready = None;
                let mut index = 0;
                while index < waiting.len() {
                    let (stream, request, received) = &mut waiting[index];
                    if head_end(&request[..*received]).is_none() {
                        assert!(
                            *received < request.len(),
                            "fixture request headers are too large"
                        );
                        match stream.read(&mut request[*received..]) {
                            // A restarted statement abandons its read.
                            Ok(0) => {
                                waiting.swap_remove(index);
                                continue;
                            }
                            Ok(count) => *received += count,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(error) => panic!("read request headers: {error}"),
                        }
                    }
                    if let Some(end) = head_end(&request[..*received]) {
                        ready = Some((index, end));
                        break;
                    }
                    index += 1;
                }
                let Some((index, header_end)) = ready else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                let (mut stream, request, received) = waiting.swap_remove(index);
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(crate::test_wait::HANG))
                    .unwrap();
                stream
                    .set_write_timeout(Some(crate::test_wait::HANG))
                    .unwrap();
                let request = *request;
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
                    length <= backing.response_capacity(),
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
                    reply_object_list(&mut backing, &mut stream, query);
                    continue;
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
                        match backing.put(key, &body, condition) {
                            Ok(tag) => {
                                server_uploads.fetch_add(1, Ordering::Relaxed);
                                while server_hold_upload_response.load(Ordering::Acquire)
                                    && !server_stop.load(Ordering::Acquire)
                                {
                                    std::thread::sleep(Duration::from_millis(1));
                                }
                                if server_stop.load(Ordering::Acquire) {
                                    break;
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
                        backing.delete(key).unwrap();
                        stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    }
                    "GET" => {
                        let metadata = match backing.get(key, None) {
                            Ok(metadata) => metadata,
                            Err(error) if error.is_not_found() => {
                                reply_object_error(&mut stream, 404, "NoSuchKey", "object missing");
                                continue;
                            }
                            Err(error) => panic!("fixture object read failed: {error}"),
                        };
                        let data = backing.body_bytes();
                        server_requests.fetch_add(1, Ordering::Relaxed);
                        server_fetched.lock().unwrap().insert(key.to_owned());
                        while server_hold_response.load(Ordering::Acquire)
                            && !server_stop.load(Ordering::Acquire)
                        {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        if server_stop.load(Ordering::Acquire) {
                            break;
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
                                continue;
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
