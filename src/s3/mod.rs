//! S3-compatible object storage client: hand-rolled HTTP/1.1 over a
//! blocking, keep-alive TCP connection, signed with SigV4. Plaintext HTTP —
//! development targets MinIO; TLS to public endpoints is an explicitly
//! deferred decision (never hand-rolled).
//!
//! Request heads are assembled in a fixed buffer; bodies are written
//! straight from the caller's slice, so object size is not bounded by any
//! client buffer. Response bodies must fit the fixed response buffer —
//! reads use ranged GETs sized accordingly.

pub mod hmac;
pub mod sha256;
pub mod sigv4;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buf::FixedBuf;
use crate::stack_format;
use crate::util::StackStr;

use sha256::{sha256, HexDigest};
use sigv4::{sign, uri_encode, SigningInput};

pub const EMPTY_SHA256_HEX: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const MAX_ATTEMPTS: u32 = 3;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum S3SetupError {
    Budget(BudgetError),
    Resolve(String, std::io::Error),
}

impl std::fmt::Display for S3SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "{e}"),
            Self::Resolve(endpoint, e) => write!(f, "cannot resolve s3_endpoint '{endpoint}': {e}"),
        }
    }
}

impl std::error::Error for S3SetupError {}

impl From<BudgetError> for S3SetupError {
    fn from(e: BudgetError) -> Self {
        Self::Budget(e)
    }
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "error text is carried inline on the stack; boxing would heap-allocate"
)]
pub enum S3Error {
    /// Non-2xx status; message holds the beginning of the error body.
    Status { code: u16, message: StackStr<256> },
    /// Connection-level failure after retries.
    Io { context: &'static str, kind: std::io::ErrorKind },
    /// Response exceeded the fixed response buffer.
    ResponseTooLarge { content_length: usize, capacity: usize },
    /// Malformed HTTP from the server.
    Protocol(&'static str),
}

impl S3Error {
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Status { code: 404, .. })
    }

    pub fn is_precondition_failed(&self) -> bool {
        matches!(self, Self::Status { code: 412 | 409, .. })
    }
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { code, message } => {
                write!(f, "object store returned {code}: {}", message.as_str())
            }
            Self::Io { context, kind } => write!(f, "object store i/o ({context}): {kind:?}"),
            Self::ResponseTooLarge {
                content_length,
                capacity,
            } => write!(
                f,
                "object store response of {content_length} bytes exceeds buffer of {capacity}"
            ),
            Self::Protocol(what) => write!(f, "object store protocol error: {what}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Precondition<'a> {
    None,
    /// `If-None-Match: *` — create only.
    IfNoneMatchAny,
    /// `If-Match: <etag>` — compare-and-swap.
    IfMatch(&'a str),
}

#[derive(Debug)]
pub struct GetResult {
    pub len: usize,
    pub etag: StackStr<80>,
}

pub struct S3Client {
    host_header: String,
    /// Resolved once at startup: `TcpStream::connect` on a string would
    /// allocate (ToSocketAddrs builds a Vec), which is forbidden after the
    /// freeze.
    connect_addr: std::net::SocketAddr,
    bucket: String,
    key_prefix: String,
    region: String,
    access_key: String,
    secret_key: String,
    stream: Option<TcpStream>,
    head: FixedBuf,
    body: FixedBuf,
    clock: fn() -> i64,
}

fn system_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64
}

impl S3Client {
    pub fn budget_bytes(config: &Config) -> usize {
        config.s3_head_bytes + config.s3_response_bytes
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, S3SetupError> {
        let host_header = config.s3_endpoint.clone();
        let connect_addr = {
            use std::net::ToSocketAddrs;
            config
                .s3_endpoint
                .to_socket_addrs()
                .map_err(|e| S3SetupError::Resolve(config.s3_endpoint.clone(), e))?
                .next()
                .ok_or_else(|| {
                    S3SetupError::Resolve(
                        config.s3_endpoint.clone(),
                        std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses"),
                    )
                })?
        };
        Ok(Self {
            host_header,
            connect_addr,
            bucket: config.s3_bucket.clone(),
            key_prefix: config.s3_prefix.clone(),
            region: config.s3_region.clone(),
            access_key: config.s3_access_key.clone(),
            secret_key: config.s3_secret_key.clone(),
            stream: None,
            head: FixedBuf::new(budget, "s3_head", config.s3_head_bytes)?,
            body: FixedBuf::new(budget, "s3_response", config.s3_response_bytes)?,
            clock: system_clock,
        })
    }

    #[cfg(test)]
    pub fn with_clock(&mut self, clock: fn() -> i64) {
        self.clock = clock;
    }

    /// Uploads an object. Returns its ETag.
    pub fn put(
        &mut self,
        key: &str,
        body: &[u8],
        precondition: Precondition,
    ) -> Result<StackStr<80>, S3Error> {
        let payload_hash = HexDigest::of(&sha256(body));
        let result = self.request("PUT", key, "", body, payload_hash.as_str(), precondition, None)?;
        Ok(result.etag)
    }

    /// Uploads an object whose body is produced by `write_body` (e.g. SST
    /// entries streamed straight from the row heap) — no client buffer
    /// bounds the object size. The caller precomputes length and SHA-256;
    /// `write_body` may run once per retry attempt.
    pub fn put_streamed(
        &mut self,
        key: &str,
        content_length: u64,
        payload_sha256_hex: &str,
        precondition: Precondition,
        mut write_body: impl FnMut(&mut TcpStream) -> std::io::Result<()>,
    ) -> Result<StackStr<80>, S3Error> {
        let mut last: Option<S3Error> = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                self.stream = None;
                std::thread::sleep(Duration::from_millis(100 << attempt));
            }
            let sent = self.send_head_and_connect(
                "PUT",
                key,
                "",
                content_length,
                payload_sha256_hex,
                precondition,
                None,
            );
            let result = sent.and_then(|()| {
                let stream = self.stream.as_mut().expect("connected");
                write_body(stream)
                    .and_then(|()| stream.flush())
                    .map_err(|e| S3Error::Io { context: "send body", kind: e.kind() })?;
                self.head.clear();
                self.body.clear();
                read_response(stream, &mut self.head, &mut self.body)
            });
            match result {
                Ok(r) => return Ok(r.etag),
                Err(e @ S3Error::Io { .. }) => {
                    self.stream = None;
                    last = Some(e);
                }
                Err(e) => {
                    self.stream = None;
                    return Err(e);
                }
            }
        }
        Err(last.expect("at least one attempt ran"))
    }

    /// Downloads an object (or a byte range, inclusive). The bytes are in
    /// [`Self::body_bytes`] afterwards.
    pub fn get(&mut self, key: &str, range: Option<(u64, u64)>) -> Result<GetResult, S3Error> {
        self.request(
            "GET",
            key,
            "",
            &[],
            EMPTY_SHA256_HEX,
            Precondition::None,
            range,
        )
    }

    pub fn body_bytes(&self) -> &[u8] {
        self.body.readable()
    }

    /// Largest response body this client can hold; ranged reads size
    /// themselves to it.
    pub fn response_capacity(&self) -> usize {
        self.body.capacity()
    }

    pub fn delete(&mut self, key: &str) -> Result<(), S3Error> {
        match self.request(
            "DELETE",
            key,
            "",
            &[],
            EMPTY_SHA256_HEX,
            Precondition::None,
            None,
        ) {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Lists keys under `prefix` (ListObjectsV2, following continuation
    /// tokens). Keys are yielded in S3's lexicographic order, with the
    /// client's key prefix stripped.
    pub fn list(
        &mut self,
        prefix: &str,
        mut each: impl FnMut(&str),
    ) -> Result<usize, S3Error> {
        let mut token: Option<StackStr<1024>> = None;
        let mut count = 0usize;
        loop {
            let mut query = StackStr::<1400>::new();
            {
                use core::fmt::Write;
                // Canonical order: sorted by parameter name.
                if let Some(t) = &token {
                    let _ = query.write_str("continuation-token=");
                    let _ = uri_encode(&mut query, t.as_str(), false);
                    let _ = query.write_char('&');
                }
                let _ = query.write_str("list-type=2&prefix=");
                let _ = uri_encode(&mut query, &self.key_prefix, false);
                let _ = uri_encode(&mut query, prefix, false);
                if query.is_truncated() {
                    return Err(S3Error::Protocol("list query overflow"));
                }
            }
            self.request(
                "GET",
                "",
                query.as_str(),
                &[],
                EMPTY_SHA256_HEX,
                Precondition::None,
                None,
            )?;
            let xml = core::str::from_utf8(self.body.readable())
                .map_err(|_| S3Error::Protocol("list response is not UTF-8"))?;
            let mut next_token: Option<StackStr<1024>> = None;
            if let Some(t) = extract_tag(xml, "NextContinuationToken") {
                next_token = Some(stack_format!(1024, "{}", t));
            }
            let truncated = extract_tag(xml, "IsTruncated") == Some("true");
            let mut rest = xml;
            while let Some(key) = extract_tag(rest, "Key") {
                let logical = key
                    .strip_prefix(self.key_prefix.as_str())
                    .ok_or(S3Error::Protocol("listed key outside the client prefix"))?;
                each(logical);
                count += 1;
                let after = rest.find("</Key>").expect("extract_tag found it") + 6;
                rest = &rest[after..];
            }
            if truncated {
                match next_token {
                    Some(t) => token = Some(t),
                    None => return Err(S3Error::Protocol("truncated list without token")),
                }
            } else {
                return Ok(count);
            }
        }
    }

    #[expect(clippy::too_many_arguments, reason = "internal seam shared by all verbs")]
    fn request(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        body: &[u8],
        payload_hash: &str,
        precondition: Precondition,
        range: Option<(u64, u64)>,
    ) -> Result<GetResult, S3Error> {
        let mut last: Option<S3Error> = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                self.stream = None; // reconnect
                std::thread::sleep(Duration::from_millis(100 << attempt));
            }
            match self.attempt(method, key, query, body, payload_hash, precondition, range) {
                Ok(r) => return Ok(r),
                Err(e @ S3Error::Io { .. }) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.expect("at least one attempt ran"))
    }

    #[expect(clippy::too_many_arguments, reason = "internal seam shared by all verbs")]
    fn attempt(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        body: &[u8],
        payload_hash: &str,
        precondition: Precondition,
        range: Option<(u64, u64)>,
    ) -> Result<GetResult, S3Error> {
        self.send_head_and_connect(
            method,
            key,
            query,
            body.len() as u64,
            payload_hash,
            precondition,
            range,
        )?;
        let stream = self.stream.as_mut().expect("connected above");
        let send = stream.write_all(body).and_then(|()| stream.flush());
        if let Err(e) = send {
            self.stream = None;
            return Err(S3Error::Io { context: "send body", kind: e.kind() });
        }

        // Receive: reuse `head` for the response head.
        self.head.clear();
        self.body.clear();
        let result = read_response(stream, &mut self.head, &mut self.body);
        match result {
            Ok(r) => Ok(r),
            Err(e) => {
                self.stream = None;
                Err(e)
            }
        }
    }

    /// Builds, signs, and sends the request head (connecting if needed).
    #[expect(clippy::too_many_arguments, reason = "internal seam shared by all verbs")]
    fn send_head_and_connect(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        content_length: u64,
        payload_hash: &str,
        precondition: Precondition,
        range: Option<(u64, u64)>,
    ) -> Result<(), S3Error> {
        let timestamp = sigv4::format_amz_timestamp((self.clock)());

        // Canonical URI: /bucket/prefix+key, path-encoded.
        let mut uri = StackStr::<1200>::new();
        {
            use core::fmt::Write;
            let _ = uri.write_char('/');
            let _ = uri_encode(&mut uri, &self.bucket, true);
            // An empty key targets the bucket itself (LIST); the prefix
            // applies only to object keys.
            if !key.is_empty() {
                let _ = uri.write_char('/');
                let _ = uri_encode(&mut uri, &self.key_prefix, true);
                let _ = uri_encode(&mut uri, key, true);
            }
            if uri.is_truncated() {
                return Err(S3Error::Protocol("key too long"));
            }
        }

        let headers = [
            ("host", self.host_header.as_str()),
            ("x-amz-content-sha256", payload_hash),
            ("x-amz-date", timestamp.as_str()),
        ];
        let signature = sign(
            &self.secret_key,
            &SigningInput {
                method,
                uri: uri.as_str(),
                query,
                headers: &headers,
                payload_sha256_hex: payload_hash,
                timestamp: timestamp.as_str(),
                region: &self.region,
                service: "s3",
            },
        );

        // Assemble the request head.
        self.head.clear();
        {
            use core::fmt::Write;
            let head = &mut self.head;
            let full = |r: core::fmt::Result| r.map_err(|_| S3Error::Protocol("head overflow"));
            full(write!(head, "{method} {}", uri.as_str()))?;
            if !query.is_empty() {
                full(write!(head, "?{query}"))?;
            }
            full(write!(head, " HTTP/1.1\r\nhost: {}\r\n", self.host_header))?;
            full(write!(head, "x-amz-content-sha256: {payload_hash}\r\n"))?;
            full(write!(head, "x-amz-date: {}\r\n", timestamp.as_str()))?;
            match precondition {
                Precondition::None => {}
                Precondition::IfNoneMatchAny => {
                    full(write!(head, "if-none-match: *\r\n"))?;
                }
                Precondition::IfMatch(etag) => {
                    full(write!(head, "if-match: {etag}\r\n"))?;
                }
            }
            if let Some((from, to)) = range {
                full(write!(head, "range: bytes={from}-{to}\r\n"))?;
            }
            full(write!(head, "content-length: {content_length}\r\n"))?;
            full(write!(
                head,
                "authorization: AWS4-HMAC-SHA256 Credential={}/{}/{}/s3/aws4_request, \
                 SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}\r\n\r\n",
                self.access_key,
                &timestamp.as_str()[..8],
                self.region,
                signature.hex.as_str()
            ))?;
        }

        // Send.
        let io = |context: &'static str| {
            move |e: std::io::Error| S3Error::Io {
                context,
                kind: e.kind(),
            }
        };
        if self.stream.is_none() {
            let stream = TcpStream::connect(self.connect_addr).map_err(io("connect"))?;
            stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(io("timeout"))?;
            stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(io("timeout"))?;
            stream.set_nodelay(true).map_err(io("nodelay"))?;
            self.stream = Some(stream);
        }
        let stream = self.stream.as_mut().expect("connected above");
        if let Err(e) = stream.write_all(self.head.readable()) {
            self.stream = None;
            return Err(S3Error::Io { context: "send head", kind: e.kind() });
        }
        Ok(())
    }
}

/// Reads one HTTP/1.1 response; the body lands in `body`.
fn read_response(
    stream: &mut TcpStream,
    head: &mut FixedBuf,
    body: &mut FixedBuf,
) -> Result<GetResult, S3Error> {
    // Read until end of head.
    let head_end = loop {
        if let Some(pos) = find_head_end(head.readable()) {
            break pos;
        }
        let space = head.writable();
        if space.is_empty() {
            return Err(S3Error::Protocol("response head too large"));
        }
        let n = stream.read(space).map_err(|e| S3Error::Io {
            context: "read head",
            kind: e.kind(),
        })?;
        if n == 0 {
            return Err(S3Error::Io {
                context: "read head",
                kind: std::io::ErrorKind::UnexpectedEof,
            });
        }
        head.advance(n);
    };

    let (status, content_length, etag) = parse_head(&head.readable()[..head_end])?;
    let mut already = head.readable().len() - head_end;

    if content_length > body.capacity() {
        return Err(S3Error::ResponseTooLarge {
            content_length,
            capacity: body.capacity(),
        });
    }
    // Move any body bytes that arrived with the head.
    let take = already.min(content_length);
    let leftover = &head.readable()[head_end..head_end + take];
    assert!(body.append(leftover), "checked against capacity");
    already = take;

    while already < content_length {
        let space = body.writable();
        let want = (content_length - already).min(space.len());
        let n = stream.read(&mut space[..want]).map_err(|e| S3Error::Io {
            context: "read body",
            kind: e.kind(),
        })?;
        if n == 0 {
            return Err(S3Error::Io {
                context: "read body",
                kind: std::io::ErrorKind::UnexpectedEof,
            });
        }
        body.advance(n);
        already += n;
    }

    if !(200..300).contains(&status) {
        let text = core::str::from_utf8(body.readable()).unwrap_or("");
        return Err(S3Error::Status {
            code: status,
            message: stack_format!(256, "{}", text),
        });
    }
    Ok(GetResult {
        len: body.readable().len(),
        etag,
    })
}

fn find_head_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn parse_head(head: &[u8]) -> Result<(u16, usize, StackStr<80>), S3Error> {
    let text = core::str::from_utf8(head).map_err(|_| S3Error::Protocol("non-UTF-8 head"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or(S3Error::Protocol("empty response"))?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(S3Error::Protocol("not HTTP/1.x"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or(S3Error::Protocol("bad status"))?;

    let mut content_length = 0usize;
    let mut etag = StackStr::<80>::new();
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .parse()
                .map_err(|_| S3Error::Protocol("bad content-length"))?;
        } else if name.eq_ignore_ascii_case("etag") {
            etag = stack_format!(80, "{}", value.trim_matches('"'));
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }
    if chunked {
        return Err(S3Error::Protocol("chunked responses not supported"));
    }
    Ok((status, content_length, etag))
}

/// First occurrence of `<tag>text</tag>`; no entity decoding (S3 keys we
/// write are restricted to URL-safe characters).
fn extract_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = stack_format!(64, "<{}>", tag);
    let close = stack_format!(64, "</{}>", tag);
    let open_at = xml.find(open.as_str())?;
    let start = open_at + open.as_str().len();
    let end = xml[start..].find(close.as_str())? + start;
    Some(&xml[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn test_config(port: u16) -> Config {
        let mut c = Config::default_dev();
        c.s3_endpoint = format!("127.0.0.1:{port}");
        c.s3_bucket = "testbucket".to_string();
        c.s3_access_key = "AKIDEXAMPLE".to_string();
        c.s3_secret_key = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string();
        c.s3_head_bytes = 8192;
        c.s3_response_bytes = 65536;
        c
    }

    /// One-shot mock server: accepts a single request, asserts on the head,
    /// answers with a canned response.
    fn mock_server(
        respond: &'static str,
        check: impl FnOnce(&str) + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                {
                    content_length = v.parse().unwrap();
                }
                let done = line == "\r\n";
                head.push_str(&line);
                if done {
                    break;
                }
            }
            let mut body = vec![0u8; content_length];
            std::io::Read::read_exact(&mut reader, &mut body).unwrap();
            check(&head);
            stream.write_all(respond.as_bytes()).unwrap();
        });
        (port, handle)
    }

    #[test]
    fn put_signs_and_parses_etag() {
        let (port, server) = mock_server(
            "HTTP/1.1 200 OK\r\netag: \"abc123\"\r\ncontent-length: 0\r\n\r\n",
            |head| {
                assert!(head.starts_with("PUT /testbucket/sst/000001.sst HTTP/1.1\r\n"));
                assert!(head.contains("authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
                assert!(head.contains("x-amz-content-sha256: "));
                assert!(head.contains("if-none-match: *"));
            },
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.with_clock(|| 1_440_938_160);
        let etag = client
            .put("sst/000001.sst", b"hello world", Precondition::IfNoneMatchAny)
            .unwrap();
        assert_eq!(etag.as_str(), "abc123");
        server.join().unwrap();
    }

    #[test]
    fn get_reads_body_and_range_header() {
        let (port, server) = mock_server(
            "HTTP/1.1 206 Partial Content\r\ncontent-length: 5\r\netag: \"e\"\r\n\r\nhello",
            |head| {
                assert!(head.contains("range: bytes=10-14\r\n"));
            },
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.with_clock(|| 1_440_938_160);
        let got = client.get("k", Some((10, 14))).unwrap();
        assert_eq!(got.len, 5);
        assert_eq!(client.body_bytes(), b"hello");
        server.join().unwrap();
    }

    #[test]
    fn non_2xx_is_a_status_error() {
        let (port, server) = mock_server(
            "HTTP/1.1 404 Not Found\r\ncontent-length: 24\r\n\r\n<Error>NoSuchKey</Error>",
            |_| {},
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.with_clock(|| 1_440_938_160);
        let err = client.get("missing", None).unwrap_err();
        assert!(err.is_not_found(), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn list_parses_keys_xml() {
        let xml = "<?xml version=\"1.0\"?><ListBucketResult>\
                   <IsTruncated>false</IsTruncated>\
                   <Contents><Key>wal/000001</Key></Contents>\
                   <Contents><Key>wal/000002</Key></Contents>\
                   </ListBucketResult>";
        let respond: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
                xml.len(),
                xml
            )
            .into_boxed_str(),
        );
        let (port, server) = mock_server(respond, |head| {
            assert!(head.contains("GET /testbucket?list-type=2&prefix=wal%2F HTTP/1.1"));
        });
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.with_clock(|| 1_440_938_160);
        let mut keys = Vec::new();
        let n = client.list("wal/", |k| keys.push(k.to_string())).unwrap();
        assert_eq!(n, 2);
        assert_eq!(keys, ["wal/000001", "wal/000002"]);
        server.join().unwrap();
    }
}
