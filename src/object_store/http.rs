//! Direct S3-compatible object-storage client: hand-rolled HTTP/1.1 over a
//! keep-alive TCP connection, with Signature Version 4 authentication.
//! Plaintext HTTP or TLS uses the isolated [`tls`] door. Every qualified
//! endpoint receives the same protocol; there are no provider branches.
//!
//! Request heads are assembled in a fixed buffer; bodies are written
//! straight from the caller's slice, so object size is not bounded by any
//! client buffer. Response bodies must fit the fixed response buffer —
//! reads use ranged GETs sized accordingly.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::config::{Config, ObjectStoreAddressing};
use crate::crypto::sha256::{HexDigest, sha256};
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::object_store::{ByteRange, EntityTag, Error, GetResult, Precondition};
use crate::stack_format;
use crate::util::StackStr;

use super::signature_v4::{SigningInput, format_timestamp, sign, signed_headers, uri_encode};

type S3Error = Error;

const MAX_ATTEMPTS: u32 = 3;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OBJECT_KEY_BYTES: usize = 1024;
const EMPTY_SHA256_HEX: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug)]
pub enum S3SetupError {
    Budget(BudgetError),
    Endpoint(&'static str),
    Bucket(&'static str),
    Region,
    AccessKey,
    SecretKey,
    SessionToken,
    Addressing(&'static str),
    Resolve(String, std::io::Error),
    Tls(String),
}

impl std::fmt::Display for S3SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "{e}"),
            Self::Endpoint(message) => write!(f, "bad object_store_endpoint: {message}"),
            Self::Bucket(message) => write!(f, "bad object_store_bucket: {message}"),
            Self::Region => write!(f, "bad object_store_region"),
            Self::AccessKey => write!(f, "bad object_store_access_key"),
            Self::SecretKey => write!(f, "bad object_store_secret_key"),
            Self::SessionToken => write!(f, "bad object_store_session_token"),
            Self::Addressing(message) => write!(f, "bad object_store_addressing: {message}"),
            Self::Resolve(endpoint, e) => {
                write!(f, "cannot resolve object_store_endpoint '{endpoint}': {e}")
            }
            Self::Tls(message) => write!(f, "tls: {message}"),
        }
    }
}

impl std::error::Error for S3SetupError {}

impl From<BudgetError> for S3SetupError {
    fn from(e: BudgetError) -> Self {
        Self::Budget(e)
    }
}

/// A parsed object-store authority. The wire Host header, TCP target, and TLS
/// server name derive from this one value instead of independently slicing a
/// free-form endpoint string.
struct Endpoint<'a> {
    authority: &'a str,
    tls_host: &'a str,
    port: &'a str,
    ipv6: bool,
}

impl<'a> Endpoint<'a> {
    fn parse(authority: &'a str) -> Result<Self, &'static str> {
        if authority.is_empty() {
            return Err("authority is empty");
        }
        if authority.contains(['/', '?', '#', '@']) || authority.contains("://") {
            return Err("use host:port, not a URL or path");
        }
        if let Some(rest) = authority.strip_prefix('[') {
            let (host, port) = rest
                .split_once("]:")
                .ok_or("IPv6 authority must be [host]:port")?;
            if host.is_empty() {
                return Err("host is empty");
            }
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err("invalid IPv6 host");
            }
            parse_port(port)?;
            return Ok(Self {
                authority,
                tls_host: host,
                port,
                ipv6: true,
            });
        }
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or("authority must include a port")?;
        if host.is_empty() || host.contains(':') {
            return Err("IPv6 authority must be bracketed and include a port");
        }
        if host.starts_with('.')
            || host.ends_with('.')
            || host.contains("..")
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
        {
            return Err("host must be a DNS name or IP address");
        }
        parse_port(port)?;
        Ok(Self {
            authority,
            tls_host: host,
            port,
            ipv6: false,
        })
    }
}

fn parse_port(port: &str) -> Result<(), &'static str> {
    match port.parse::<u16>() {
        Ok(1..) => Ok(()),
        _ => Err("port must be in 1..=65535"),
    }
}

use crate::object_store::tls;

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
    session_token: String,
    addressing: ObjectStoreAddressing,
    stream: Option<tls::Transport>,
    /// TLS client state when object-store TLS is on (built at startup).
    tls_context: Option<tls::TlsContext>,
    head: FixedBuf,
    body: FixedBuf,
    clock: fn() -> i64,
    /// A non-blocking GET in progress: the response is being read
    /// incrementally, advanced by the reactor when the socket is ready.
    pending: Option<PendingResponse>,
    async_gets: bool,
}

/// Incremental HTTP-response state for a non-blocking GET. The request was
/// sent (blocking write — fast); the response is read in chunks via
/// [`S3Client::advance_pending`], driven by reactor readability events.
struct PendingResponse {
    head_end: Option<usize>,
    status: u16,
    content_length: usize,
    body_read: usize,
}

/// HTTP response metadata shared by verbs which either require an object
/// generation (GET/PUT) or intentionally discard it (LIST/DELETE).
struct Response {
    len: usize,
    etag: Option<EntityTag>,
}

/// A parsed HTTP response head. A body has exactly one framing rule.
struct ResponseHead {
    status: u16,
    etag: Option<EntityTag>,
    framing: BodyFraming,
}

enum BodyFraming {
    ContentLength(usize),
    Chunked,
    Empty,
}

fn system_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64
}

fn header_value(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn valid_bucket(bucket: &str) -> bool {
    let bytes = bucket.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && !bucket.contains("..")
        && !bucket.split('.').all(|part| part.parse::<u8>().is_ok())
}

impl S3Client {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        config.object_store_head_bytes + config.object_store_response_bytes
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, S3SetupError> {
        if !valid_bucket(&config.object_store_bucket) {
            return Err(S3SetupError::Bucket(
                "must be a 3..=63 byte DNS-compatible S3 bucket name",
            ));
        }
        if !header_value(&config.object_store_region)
            || config.object_store_region.len() > 64
            || config.object_store_region.contains('/')
        {
            return Err(S3SetupError::Region);
        }
        if !header_value(&config.object_store_access_key)
            || config.object_store_access_key.len() > 128
            || config.object_store_access_key.contains(['/', ','])
        {
            return Err(S3SetupError::AccessKey);
        }
        if config.object_store_secret_key.is_empty() || config.object_store_secret_key.len() > 124 {
            return Err(S3SetupError::SecretKey);
        }
        if (!config.object_store_session_token.is_empty()
            && !header_value(&config.object_store_session_token))
            || config.object_store_session_token.len() > 2048
        {
            return Err(S3SetupError::SessionToken);
        }
        let endpoint =
            Endpoint::parse(&config.object_store_endpoint).map_err(S3SetupError::Endpoint)?;
        let (host_header, tls_host) = match config.object_store_addressing {
            ObjectStoreAddressing::Path => (
                endpoint.authority.to_string(),
                endpoint.tls_host.to_string(),
            ),
            ObjectStoreAddressing::VirtualHosted => {
                if endpoint.ipv6 || endpoint.tls_host.parse::<std::net::Ipv4Addr>().is_ok() {
                    return Err(S3SetupError::Addressing(
                        "virtual_hosted requires a DNS endpoint",
                    ));
                }
                (
                    format!(
                        "{}.{}:{}",
                        config.object_store_bucket, endpoint.tls_host, endpoint.port
                    ),
                    format!("{}.{}", config.object_store_bucket, endpoint.tls_host),
                )
            }
        };
        let connect_addr = {
            use std::net::ToSocketAddrs;
            config
                .object_store_endpoint
                .to_socket_addrs()
                .map_err(|e| S3SetupError::Resolve(config.object_store_endpoint.clone(), e))?
                .next()
                .ok_or_else(|| {
                    S3SetupError::Resolve(
                        config.object_store_endpoint.clone(),
                        std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses"),
                    )
                })?
        };
        Ok(Self {
            host_header,
            connect_addr,
            bucket: config.object_store_bucket.clone(),
            key_prefix: config.object_store_prefix.clone(),
            region: config.object_store_region.clone(),
            access_key: config.object_store_access_key.clone(),
            secret_key: config.object_store_secret_key.clone(),
            session_token: config.object_store_session_token.clone(),
            addressing: config.object_store_addressing,
            stream: None,
            tls_context: if config.object_store_tls {
                Some(
                    tls::build_context(&tls_host, &config.object_store_tls_ca_file)
                        .map_err(S3SetupError::Tls)?,
                )
            } else {
                None
            },
            head: FixedBuf::new(budget, "object_store_head", config.object_store_head_bytes)?,
            body: FixedBuf::new(
                budget,
                "object_store_response",
                config.object_store_response_bytes,
            )?,
            clock: system_clock,
            pending: None,
            async_gets: false,
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
    ) -> Result<EntityTag, S3Error> {
        let payload_hash = HexDigest::of(&sha256(body));
        let result = self.request(
            "PUT",
            key,
            "",
            body,
            payload_hash.as_str(),
            precondition,
            None,
        )?;
        result
            .etag
            .ok_or(S3Error::Protocol("PUT response missing ETag"))
    }

    /// Downloads an object (or a byte range, inclusive). The bytes are in
    /// [`Self::body_bytes`] afterwards. When a non-blocking GET is in
    /// progress, advances it instead of starting a new request.
    pub fn get(&mut self, key: &str, range: Option<ByteRange>) -> Result<GetResult, S3Error> {
        if self.pending.is_some() {
            return self.advance_pending();
        }
        if !self.async_gets {
            let response = self.request(
                "GET",
                key,
                "",
                &[],
                EMPTY_SHA256_HEX,
                Precondition::None,
                range,
            )?;
            return Ok(GetResult {
                len: response.len,
                etag: response
                    .etag
                    .ok_or(S3Error::Protocol("GET response missing ETag"))?,
            });
        }
        // Initiate: send the request (blocking write — fast), then switch to
        // non-blocking for the response read so the reactor can serve other
        // connections while we wait.
        self.send_head_and_connect(
            "GET",
            key,
            "",
            0,
            EMPTY_SHA256_HEX,
            Precondition::None,
            range,
        )?;
        let stream = self.stream.as_mut().expect("connected above");
        let send = stream.write_all(&[]).and_then(|()| stream.flush());
        if let Err(e) = send {
            self.stream = None;
            return Err(S3Error::Io {
                context: "send body",
                kind: e.kind(),
                detail: StackStr::new(),
            });
        }
        // Switch to non-blocking for the response read.
        if let Err(e) = self.stream.as_ref().unwrap().set_nonblocking(true) {
            self.stream = None;
            return Err(S3Error::Io {
                context: "set_nonblocking",
                kind: e.kind(),
                detail: StackStr::new(),
            });
        }
        self.head.clear();
        self.body.clear();
        self.pending = Some(PendingResponse {
            head_end: None,
            status: 0,
            content_length: 0,
            body_read: 0,
        });
        // Try to read immediately (data might already be available).
        self.advance_pending()
    }

    /// Enables reactor-driven object reads. This is configured once by the
    /// server for the block-store client; all other clients retain blocking
    /// request semantics.
    pub fn enable_async_gets(&mut self) {
        self.async_gets = true;
    }

    pub fn disable_async_gets(&mut self) {
        assert!(
            self.pending.is_none(),
            "cannot switch a pending GET to blocking"
        );
        self.async_gets = false;
    }

    /// Whether a non-blocking GET is in flight.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The raw socket fd of the in-flight GET, for reactor registration.
    pub fn pending_fd(&self) -> Option<std::os::fd::RawFd> {
        if self.pending.is_some() {
            self.stream.as_ref().map(|s| s.raw_fd())
        } else {
            None
        }
    }

    /// Clears a pending GET (used by PUT/DELETE/LIST paths that need the
    /// connection: drops it so the next request reconnects).
    pub fn clear_pending(&mut self) {
        if self.pending.is_some() {
            self.pending = None;
            self.stream = None; // force reconnect
        }
    }

    /// Reads more of the pending GET response. Returns `Ok` when the full
    /// response is available, or `Err(WouldBlock)` when more data is needed.
    pub fn advance_pending(&mut self) -> Result<GetResult, S3Error> {
        let stream = self.stream.as_mut().expect("pending implies connected");
        let pending = self.pending.as_mut().expect("pending set above");

        // Phase 1: read the HTTP head (until \r\n\r\n).
        if pending.head_end.is_none() {
            loop {
                if let Some(pos) = find_head_end(self.head.readable()) {
                    pending.head_end = Some(pos);
                    break;
                }
                let space = self.head.writable();
                if space.is_empty() {
                    self.clear_pending();
                    return Err(S3Error::Protocol("response head too large"));
                }
                match stream.read(space) {
                    Ok(0) => {
                        self.clear_pending();
                        return Err(S3Error::Io {
                            context: "read head",
                            kind: std::io::ErrorKind::UnexpectedEof,
                            detail: StackStr::new(),
                        });
                    }
                    Ok(n) => self.head.advance(n),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        return Err(S3Error::WouldBlock);
                    }
                    Err(e) => {
                        self.clear_pending();
                        return Err(S3Error::Io {
                            context: "read head",
                            kind: e.kind(),
                            detail: StackStr::new(),
                        });
                    }
                }
            }
        }

        // Phase 2: parse the head (once).
        let head_end = pending.head_end.unwrap();
        let response_head = match parse_head(&self.head.readable()[..head_end]) {
            Ok(head) => head,
            Err(error) => {
                self.clear_pending();
                return Err(error);
            }
        };
        let content_length = match response_head.framing {
            BodyFraming::ContentLength(length) => length,
            BodyFraming::Empty => 0,
            BodyFraming::Chunked => {
                self.clear_pending();
                return Err(S3Error::Protocol(
                    "chunked encoding not supported in non-blocking GET",
                ));
            }
        };
        pending.status = response_head.status;
        pending.content_length = content_length;

        if content_length > self.body.capacity() {
            self.clear_pending();
            return Err(S3Error::ResponseTooLarge {
                content_length,
                capacity: self.body.capacity(),
            });
        }

        // Move any body bytes that arrived with the head (first read only).
        if pending.body_read == 0 {
            let already = self.head.readable().len() - head_end;
            let take = already.min(content_length);
            if take > 0 {
                assert!(
                    self.body
                        .append(&self.head.readable()[head_end..head_end + take]),
                    "checked against capacity"
                );
                pending.body_read = take;
            }
        }

        // Phase 3: read the body.
        while pending.body_read < content_length {
            let space = self.body.writable();
            let want = (content_length - pending.body_read).min(space.len());
            match stream.read(&mut space[..want]) {
                Ok(0) => {
                    self.clear_pending();
                    return Err(S3Error::Io {
                        context: "read body",
                        kind: std::io::ErrorKind::UnexpectedEof,
                        detail: StackStr::new(),
                    });
                }
                Ok(n) => {
                    self.body.advance(n);
                    pending.body_read += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    return Err(S3Error::WouldBlock);
                }
                Err(e) => {
                    self.clear_pending();
                    return Err(S3Error::Io {
                        context: "read body",
                        kind: e.kind(),
                        detail: StackStr::new(),
                    });
                }
            }
        }

        // Phase 4: complete. Deregister from the reactor and restore blocking.
        if self
            .stream
            .as_ref()
            .unwrap()
            .set_nonblocking(false)
            .is_err()
        {
            // The response is complete, but this connection is no longer safe
            // for a later blocking request.
            self.stream = None;
        }
        self.pending = None;
        if !(200..300).contains(&response_head.status) {
            return Err(status_error(response_head.status, self.body.readable()));
        }
        Ok(GetResult {
            len: self.body.readable().len(),
            etag: response_head
                .etag
                .ok_or(S3Error::Protocol("GET response missing ETag"))?,
        })
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

    /// Lists all logical keys under `prefix` through paginated ListObjectsV2.
    pub fn list(&mut self, prefix: &str, mut each: impl FnMut(&str)) -> Result<usize, S3Error> {
        if self.key_prefix.len() + prefix.len() > MAX_OBJECT_KEY_BYTES {
            return Err(S3Error::Protocol("list prefix exceeds S3 key limit"));
        }
        let mut count = 0;
        let mut continuation: Option<StackStr<1024>> = None;
        let mut last_key: Option<StackStr<1024>> = None;
        loop {
            let mut query = StackStr::<6400>::new();
            {
                use core::fmt::Write;
                if let Some(token) = &continuation {
                    let _ = query.write_str("continuation-token=");
                    let _ = uri_encode(&mut query, token.as_str(), false);
                    let _ = query.write_char('&');
                }
                let _ = query.write_str("encoding-type=url&list-type=2&prefix=");
                let _ = uri_encode(&mut query, &self.key_prefix, false);
                let _ = uri_encode(&mut query, prefix, false);
            }
            if query.is_truncated() {
                return Err(S3Error::Protocol("list query overflow"));
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
            let truncated = match extract_xml_text(xml, "IsTruncated") {
                Some("true") => true,
                Some("false") => false,
                Some(_) => return Err(S3Error::Protocol("invalid IsTruncated value")),
                None => return Err(S3Error::Protocol("list response missing IsTruncated")),
            };
            let next = extract_xml_text(xml, "NextContinuationToken")
                .map(decode_xml_text::<1024>)
                .transpose()?;
            let mut rest = xml;
            while let Some(encoded_key) = extract_xml_text(rest, "Key") {
                let key = percent_decode::<MAX_OBJECT_KEY_BYTES>(encoded_key)?;
                let logical = key
                    .as_str()
                    .strip_prefix(self.key_prefix.as_str())
                    .ok_or(S3Error::Protocol("listed key outside configured prefix"))?;
                if !logical.starts_with(prefix) {
                    return Err(S3Error::Protocol("listed key outside requested prefix"));
                }
                if last_key
                    .as_ref()
                    .is_some_and(|previous| logical <= previous.as_str())
                {
                    return Err(S3Error::Protocol("listed keys are not strictly ordered"));
                }
                each(logical);
                last_key = Some(StackStr::from_str(logical));
                count += 1;
                let after = rest
                    .find("</Key>")
                    .ok_or(S3Error::Protocol("unterminated list key"))?
                    + 6;
                rest = &rest[after..];
            }
            if !truncated {
                return Ok(count);
            }
            let next = next.ok_or(S3Error::Protocol(
                "truncated list without continuation token",
            ))?;
            if continuation.as_ref() == Some(&next) {
                return Err(S3Error::Protocol("list continuation token did not advance"));
            }
            continuation = Some(next);
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal seam shared by every S3 operation"
    )]
    fn request(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        body: &[u8],
        payload_hash: &str,
        precondition: Precondition,
        range: Option<ByteRange>,
    ) -> Result<Response, S3Error> {
        // Drop any pending non-blocking GET so the connection is clean.
        self.clear_pending();
        let mut last: Option<S3Error> = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                self.stream = None; // reconnect
                std::thread::sleep(Duration::from_millis(100 << attempt));
            }
            match self.attempt(method, key, query, body, payload_hash, precondition, range) {
                Ok(r) => return Ok(r),
                Err(e @ S3Error::Io { .. }) => last = Some(e),
                Err(e @ S3Error::Status { .. }) if e.is_retryable() => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.expect("at least one attempt ran"))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal seam shared by every S3 operation"
    )]
    fn attempt(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        body: &[u8],
        payload_hash: &str,
        precondition: Precondition,
        range: Option<ByteRange>,
    ) -> Result<Response, S3Error> {
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
            return Err(S3Error::Io {
                context: "send body",
                kind: e.kind(),
                detail: StackStr::new(),
            });
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

    /// Builds, signs, and sends one S3-compatible request head.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal seam shared by every S3 operation"
    )]
    fn send_head_and_connect(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        content_length: u64,
        payload_hash: &str,
        precondition: Precondition,
        range: Option<ByteRange>,
    ) -> Result<(), S3Error> {
        if self.key_prefix.len() + key.len() > MAX_OBJECT_KEY_BYTES {
            return Err(S3Error::Protocol("object key exceeds S3 key limit"));
        }
        let mut uri = StackStr::<3200>::new();
        {
            use core::fmt::Write;
            let _ = uri.write_char('/');
            if self.addressing == ObjectStoreAddressing::Path {
                let _ = uri_encode(&mut uri, &self.bucket, false);
            }
            if !key.is_empty() {
                if self.addressing == ObjectStoreAddressing::Path {
                    let _ = uri.write_char('/');
                }
                let _ = uri_encode(&mut uri, &self.key_prefix, true);
                let _ = uri_encode(&mut uri, key, true);
            }
            if uri.is_truncated() {
                return Err(S3Error::Protocol("key too long"));
            }
        }

        let timestamp = format_timestamp((self.clock)());
        let base_headers = [
            ("host", self.host_header.as_str()),
            ("x-amz-content-sha256", payload_hash),
            ("x-amz-date", timestamp.as_str()),
        ];
        let token_headers = [
            ("host", self.host_header.as_str()),
            ("x-amz-content-sha256", payload_hash),
            ("x-amz-date", timestamp.as_str()),
            ("x-amz-security-token", self.session_token.as_str()),
        ];
        let signed = if self.session_token.is_empty() {
            &base_headers[..]
        } else {
            &token_headers[..]
        };
        let signature = sign(
            &self.secret_key,
            &SigningInput {
                method,
                uri: uri.as_str(),
                query,
                headers: signed,
                payload_sha256_hex: payload_hash,
                timestamp: timestamp.as_str(),
                region: &self.region,
            },
        );
        let mut signed_names = StackStr::<96>::new();
        signed_headers(&mut signed_names, signed);
        if signed_names.is_truncated() {
            return Err(S3Error::Protocol("signed-header list overflow"));
        }

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
            if !self.session_token.is_empty() {
                full(write!(
                    head,
                    "x-amz-security-token: {}\r\n",
                    self.session_token
                ))?;
            }
            match precondition {
                Precondition::None => {}
                Precondition::IfNoneMatchAny => {
                    full(write!(head, "if-none-match: *\r\n"))?;
                }
                Precondition::IfMatch(etag) => {
                    full(write!(head, "if-match: {}\r\n", etag.as_str()))?;
                }
            }
            if let Some(range) = range {
                full(write!(
                    head,
                    "range: bytes={}-{}\r\n",
                    range.first(),
                    range.last()
                ))?;
            }
            full(write!(head, "content-length: {content_length}\r\n"))?;
            full(write!(
                head,
                "authorization: AWS4-HMAC-SHA256 Credential={}/{}/{}/s3/aws4_request, SignedHeaders={}, Signature={}\r\n\r\n",
                self.access_key,
                &timestamp.as_str()[..8],
                self.region,
                signed_names.as_str(),
                signature.hex.as_str()
            ))?;
        }

        // Send.
        let io = |context: &'static str| {
            move |e: std::io::Error| S3Error::Io {
                context,
                kind: e.kind(),
                detail: StackStr::new(),
            }
        };
        if self.stream.is_none() {
            let stream = TcpStream::connect(self.connect_addr).map_err(io("connect"))?;
            stream
                .set_read_timeout(Some(IO_TIMEOUT))
                .map_err(io("timeout"))?;
            stream
                .set_write_timeout(Some(IO_TIMEOUT))
                .map_err(io("timeout"))?;
            stream.set_nodelay(true).map_err(io("nodelay"))?;
            self.stream = Some(match &self.tls_context {
                Some(context) => tls::Transport::tls(stream, &context.config, &context.server_name)
                    .map_err(io("tls"))?,
                None => tls::Transport::plain(stream),
            });
        }
        let stream = self.stream.as_mut().expect("connected above");
        if let Err(e) = stream.write_all(self.head.readable()) {
            self.stream = None;
            return Err(S3Error::Io {
                context: "send head",
                kind: e.kind(),
                detail: StackStr::new(),
            });
        }
        Ok(())
    }
}

/// Reads one HTTP/1.1 response; the body lands in `body`.
fn read_response(
    stream: &mut tls::Transport,
    head: &mut FixedBuf,
    body: &mut FixedBuf,
) -> Result<Response, S3Error> {
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
            detail: StackStr::new(),
        })?;
        if n == 0 {
            return Err(S3Error::Io {
                context: "read head",
                kind: std::io::ErrorKind::UnexpectedEof,
                detail: StackStr::new(),
            });
        }
        head.advance(n);
    };

    let response_head = parse_head(&head.readable()[..head_end])?;

    match response_head.framing {
        BodyFraming::Chunked => read_chunked_body(stream, &head.readable()[head_end..], body)?,
        BodyFraming::Empty => {}
        BodyFraming::ContentLength(content_length) => {
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
                    detail: StackStr::new(),
                })?;
                if n == 0 {
                    return Err(S3Error::Io {
                        context: "read body",
                        kind: std::io::ErrorKind::UnexpectedEof,
                        detail: StackStr::new(),
                    });
                }
                body.advance(n);
                already += n;
            }
        }
    }

    if !(200..300).contains(&response_head.status) {
        return Err(status_error(response_head.status, body.readable()));
    }
    Ok(Response {
        len: body.readable().len(),
        etag: response_head.etag,
    })
}

/// Decodes a `Transfer-Encoding: chunked` body into `body`: hex-sized chunks
/// separated by CRLF, a zero-size chunk ending the stream (trailers, if any,
/// are read to their final CRLF and dropped). The decoded body is still
/// bounded by the response buffer — a loud [`S3Error::ResponseTooLarge`], as
/// for a plain body.
fn read_chunked_body(
    stream: &mut tls::Transport,
    leftover: &[u8],
    body: &mut FixedBuf,
) -> Result<(), S3Error> {
    // Bytes that arrived with the head drain first, then the socket.
    struct Feed<'a> {
        leftover: &'a [u8],
        stream: &'a mut tls::Transport,
    }
    impl Feed<'_> {
        fn read(&mut self, out: &mut [u8]) -> Result<usize, S3Error> {
            if !self.leftover.is_empty() {
                let n = self.leftover.len().min(out.len());
                out[..n].copy_from_slice(&self.leftover[..n]);
                self.leftover = &self.leftover[n..];
                return Ok(n);
            }
            self.stream.read(out).map_err(|e| S3Error::Io {
                context: "read chunk",
                kind: e.kind(),
                detail: StackStr::new(),
            })
        }
    }
    fn fill(feed: &mut Feed, carry: &mut [u8; 512], carry_len: &mut usize) -> Result<(), S3Error> {
        let n = feed.read(&mut carry[*carry_len..])?;
        if n == 0 {
            return Err(S3Error::Io {
                context: "read chunk",
                kind: std::io::ErrorKind::UnexpectedEof,
                detail: StackStr::new(),
            });
        }
        *carry_len += n;
        Ok(())
    }
    let mut feed = Feed { leftover, stream };
    // A small carry window for chunk framing (size lines, CRLFs, trailers);
    // chunk payloads copy straight into `body`.
    let mut carry = [0u8; 512];
    let mut carry_len = 0usize;
    loop {
        // Read the size line (hex, optional extensions after ';').
        let line_end = loop {
            if let Some(p) = carry[..carry_len].windows(2).position(|w| w == b"\r\n") {
                break p;
            }
            if carry_len == carry.len() {
                return Err(S3Error::Protocol("chunk size line too long"));
            }
            fill(&mut feed, &mut carry, &mut carry_len)?;
        };
        let line = core::str::from_utf8(&carry[..line_end])
            .map_err(|_| S3Error::Protocol("non-UTF-8 chunk size"))?;
        let hex = line.split(';').next().unwrap_or("").trim();
        let size =
            usize::from_str_radix(hex, 16).map_err(|_| S3Error::Protocol("bad chunk size"))?;
        // Drop the size line from the carry.
        carry.copy_within(line_end + 2..carry_len, 0);
        carry_len -= line_end + 2;

        if size == 0 {
            // Trailers (if any) end with an empty line; the carry may already
            // hold it.
            loop {
                if carry[..carry_len].starts_with(b"\r\n")
                    || carry[..carry_len].windows(4).any(|w| w == b"\r\n\r\n")
                {
                    return Ok(());
                }
                if carry_len == carry.len() {
                    return Err(S3Error::Protocol("chunk trailers too long"));
                }
                fill(&mut feed, &mut carry, &mut carry_len)?;
            }
        }

        // Chunk payload: first whatever the carry holds, then the feed.
        let mut remaining = size;
        let from_carry = remaining.min(carry_len);
        if !body.append(&carry[..from_carry]) {
            return Err(S3Error::ResponseTooLarge {
                content_length: body.readable().len() + remaining,
                capacity: body.capacity(),
            });
        }
        carry.copy_within(from_carry..carry_len, 0);
        carry_len -= from_carry;
        remaining -= from_carry;
        while remaining > 0 {
            let space = body.writable();
            if space.is_empty() {
                return Err(S3Error::ResponseTooLarge {
                    content_length: body.readable().len() + remaining,
                    capacity: body.capacity(),
                });
            }
            let want = remaining.min(space.len());
            let n = feed.read(&mut space[..want])?;
            if n == 0 {
                return Err(S3Error::Io {
                    context: "read chunk",
                    kind: std::io::ErrorKind::UnexpectedEof,
                    detail: StackStr::new(),
                });
            }
            body.advance(n);
            remaining -= n;
        }
        // The chunk's trailing CRLF.
        while carry_len < 2 {
            fill(&mut feed, &mut carry, &mut carry_len)?;
        }
        if &carry[..2] != b"\r\n" {
            return Err(S3Error::Protocol("chunk missing its trailing CRLF"));
        }
        carry.copy_within(2..carry_len, 0);
        carry_len -= 2;
    }
}

fn find_head_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

fn extract_xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = stack_format!(64, "<{}>", tag);
    let close = stack_format!(64, "</{}>", tag);
    let open_at = xml.find(open.as_str())?;
    let start = open_at + open.as_str().len();
    let end = xml[start..].find(close.as_str())? + start;
    Some(&xml[start..end])
}

fn status_error(code: u16, body: &[u8]) -> S3Error {
    let Ok(xml) = core::str::from_utf8(body) else {
        return S3Error::Status {
            code,
            service_code: StackStr::new(),
            message: stack_format!(256, "non-UTF-8 error response"),
        };
    };
    let service_code = match extract_xml_text(xml, "Code") {
        Some(text) => match decode_xml_text::<64>(text) {
            Ok(code) => code,
            Err(error) => return error,
        },
        None => StackStr::new(),
    };
    let message = match extract_xml_text(xml, "Message") {
        Some(text) => match decode_xml_text::<256>(text) {
            Ok(message) => message,
            Err(error) => return error,
        },
        None => {
            let raw = StackStr::from_str(xml);
            if raw.is_truncated() {
                return S3Error::Protocol("error response exceeds fixed diagnostic buffer");
            }
            raw
        }
    };
    S3Error::Status {
        code,
        service_code,
        message,
    }
}

fn decode_xml_text<const N: usize>(input: &str) -> Result<StackStr<N>, S3Error> {
    use core::fmt::Write;
    let mut out = StackStr::<N>::new();
    let mut rest = input;
    while let Some(at) = rest.find('&') {
        out.write_str(&rest[..at])
            .map_err(|_| S3Error::Protocol("XML text exceeds fixed buffer"))?;
        rest = &rest[at..];
        let (decoded, consumed) = if rest.starts_with("&amp;") {
            ('&', 5)
        } else if rest.starts_with("&lt;") {
            ('<', 4)
        } else if rest.starts_with("&gt;") {
            ('>', 4)
        } else if rest.starts_with("&quot;") {
            ('\"', 6)
        } else if rest.starts_with("&apos;") {
            ('\'', 6)
        } else if let Some(numeric) = rest.strip_prefix("&#") {
            let end = numeric
                .find(';')
                .ok_or(S3Error::Protocol("unterminated XML character reference"))?;
            let digits = &numeric[..end];
            let (radix, digits) = digits
                .strip_prefix(['x', 'X'])
                .map_or((10, digits), |hex| (16, hex));
            let scalar = u32::from_str_radix(digits, radix)
                .ok()
                .and_then(char::from_u32)
                .ok_or(S3Error::Protocol("invalid XML character reference"))?;
            (scalar, end + 3)
        } else {
            return Err(S3Error::Protocol("unsupported XML entity"));
        };
        out.write_char(decoded)
            .map_err(|_| S3Error::Protocol("XML text exceeds fixed buffer"))?;
        rest = &rest[consumed..];
    }
    out.write_str(rest)
        .map_err(|_| S3Error::Protocol("XML text exceeds fixed buffer"))?;
    if out.is_truncated() {
        return Err(S3Error::Protocol("XML text exceeds fixed buffer"));
    }
    Ok(out)
}

fn percent_decode<const N: usize>(input: &str) -> Result<StackStr<N>, S3Error> {
    let source = input.as_bytes();
    let mut bytes = [0u8; N];
    let (mut read, mut written) = (0usize, 0usize);
    while read < source.len() {
        if written == bytes.len() {
            return Err(S3Error::Protocol("decoded key exceeds fixed buffer"));
        }
        if source[read] == b'%' {
            if read + 2 >= source.len() {
                return Err(S3Error::Protocol("truncated percent encoding"));
            }
            bytes[written] = (hex_value(source[read + 1])? << 4) | hex_value(source[read + 2])?;
            read += 3;
        } else {
            bytes[written] = source[read];
            read += 1;
        }
        written += 1;
    }
    let text = core::str::from_utf8(&bytes[..written])
        .map_err(|_| S3Error::Protocol("decoded key is not UTF-8"))?;
    Ok(StackStr::from_str(text))
}

fn hex_value(byte: u8) -> Result<u8, S3Error> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(S3Error::Protocol("bad percent encoding")),
    }
}

fn parse_head(head: &[u8]) -> Result<ResponseHead, S3Error> {
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

    let mut content_length = None;
    let mut etag = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(S3Error::Protocol("malformed response header"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(S3Error::Protocol("duplicate content-length header"));
            }
            content_length = Some(
                value
                    .parse()
                    .map_err(|_| S3Error::Protocol("bad content-length"))?,
            );
        } else if name.eq_ignore_ascii_case("etag") {
            if etag.is_some() {
                return Err(S3Error::Protocol("duplicate ETag header"));
            }
            etag = Some(EntityTag::parse(value).map_err(S3Error::Protocol)?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(S3Error::Protocol("unsupported transfer-encoding"));
            }
            chunked = true;
        }
    }
    let framing = match (content_length, chunked) {
        (Some(_), true) => return Err(S3Error::Protocol("response has conflicting framing")),
        (Some(length), false) => BodyFraming::ContentLength(length),
        (None, true) => BodyFraming::Chunked,
        (None, false) if matches!(status, 204 | 304) => BodyFraming::Empty,
        (None, false) => return Err(S3Error::Protocol("response missing body framing")),
    };
    Ok(ResponseHead {
        status,
        etag,
        framing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn test_config(port: u16) -> Config {
        let mut c = Config::default_dev();
        c.object_store_endpoint = format!("127.0.0.1:{port}");
        c.object_store_bucket = "testbucket".to_string();
        c.object_store_access_key = "AKIDEXAMPLE".to_string();
        c.object_store_secret_key = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string();
        c.object_store_head_bytes = 8192;
        c.object_store_response_bytes = 65536;
        c
    }

    fn fixture_clock() -> i64 {
        1_440_938_160 // 20150830T123600Z
    }

    /// One-shot mock server: accepts a single request, asserts on the head,
    /// answers with a canned response.
    fn mock_server(
        respond: &'static str,
        check: impl FnOnce(&str) + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let mut check = Some(check);
        mock_server_sequence(vec![respond], move |_, head| {
            check.take().unwrap()(head);
        })
    }

    fn mock_server_sequence(
        responses: Vec<&'static str>,
        mut check: impl FnMut(usize, &str) + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            for (index, respond) in responses.into_iter().enumerate() {
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
                check(index, &head);
                stream.write_all(respond.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        (port, handle)
    }

    #[test]
    fn tls_round_trip() {
        // An in-process rustls server (the dependency's own server side — no
        // new dev dependency) answers one canned S3 response over TLS; the
        // client connects with object-store TLS on, trusting the checked-in
        // self-signed certificate (provenance: tests/data/README.md).
        use std::sync::Arc;
        let cert_pem = std::fs::read_to_string("tests/data/tls-test-cert.pem").unwrap();
        let key_pem = std::fs::read_to_string("tests/data/tls-test-key.pem").unwrap();
        let cert_der = {
            let mut ders = Vec::new();
            let mut in_block = false;
            let mut b64 = String::new();
            for line in cert_pem.lines() {
                let line = line.trim();
                if line.starts_with("-----BEGIN") {
                    in_block = true;
                    b64.clear();
                } else if line.starts_with("-----END") {
                    in_block = false;
                    ders.push(b64.clone());
                } else if in_block {
                    b64.push_str(line);
                }
            }
            test_b64(&ders[0])
        };
        let key_der = {
            let mut b64 = String::new();
            let mut in_block = false;
            for line in key_pem.lines() {
                let line = line.trim();
                if line.starts_with("-----BEGIN") {
                    in_block = true;
                } else if line.starts_with("-----END") {
                    in_block = false;
                } else if in_block {
                    b64.push_str(line);
                }
            }
            test_b64(&b64)
        };
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(cert_der)],
                rustls::pki_types::PrivateKeyDer::try_from(key_der).unwrap(),
            )
            .unwrap();
        let server_config = Arc::new(server_config);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let session = rustls::ServerConnection::new(server_config).unwrap();
            let mut tls = rustls::StreamOwned::new(session, stream);
            // Read the request head (ignore its content).
            let mut buf = [0u8; 4096];
            let mut head = Vec::new();
            loop {
                let n = std::io::Read::read(&mut tls, &mut buf).unwrap();
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            std::io::Write::write_all(
                &mut tls,
                b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\netag: \"t\"\r\n\r\nover tls!",
            )
            .unwrap();
        });
        let mut config = test_config(port);
        config.object_store_tls = true;
        config.object_store_tls_ca_file = "tests/data/tls-test-cert.pem".to_string();
        // The certificate carries an IP SAN for exactly this: `localhost`
        // may resolve to ::1 while the listener binds 127.0.0.1.
        config.object_store_endpoint = format!("127.0.0.1:{port}");
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.get("k", None).unwrap();
        assert_eq!(client.body_bytes(), b"over tls!");
        handle.join().unwrap();
    }

    /// Test-local base64 (the module under test has its own in tls.rs, kept
    /// private there).
    fn test_b64(text: &str) -> Vec<u8> {
        fn value(c: u8) -> u8 {
            match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => 0xFF,
            }
        }
        let mut out = Vec::new();
        let (mut acc, mut bits) = (0u32, 0u32);
        for &c in text.as_bytes() {
            if c == b'=' {
                break;
            }
            let v = value(c);
            assert_ne!(v, 0xFF);
            acc = (acc << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        out
    }

    #[test]
    fn chunked_bodies_decode() {
        // Two data chunks (with an extension on the first size line), a zero
        // chunk, and a trailer — the portable HTTP framing clients must accept.
        let (port, server) = mock_server(
            "HTTP/1.1 200 OK\r\netag: \"chunked\"\r\ntransfer-encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nx-trailer: t\r\n\r\n",
            |_| {},
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.get("k", None).unwrap();
        assert_eq!(client.body_bytes(), b"hello world");
        server.join().unwrap();
    }

    #[test]
    fn chunked_body_overflow_is_loud() {
        // A chunk stream larger than the response buffer must refuse, not
        // truncate: the declared capacity below is 64 KiB and the single
        // chunk claims 128 KiB.
        let mut big =
            String::from("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n20000\r\n");
        big.push_str(&"y".repeat(0x20000));
        big.push_str("\r\n0\r\n\r\n");
        let leaked: &'static str = Box::leak(big.into_boxed_str());
        let (port, server) = mock_server(leaked, |_| {});
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        let err = client.get("k", None).unwrap_err();
        assert!(matches!(err, S3Error::ResponseTooLarge { .. }), "{err:?}");
        // The mock's write may fail once the client stops reading; ignore.
        let _ = server.join();
    }

    #[test]
    fn put_uses_locked_s3_profile_and_parses_etag() {
        let (port, server) = mock_server(
            "HTTP/1.1 200 OK\r\netag: \"abc123\"\r\ncontent-length: 0\r\n\r\n",
            |head| {
                assert!(head.starts_with("PUT /testbucket/sst/000001.sst HTTP/1.1\r\n"));
                assert!(head.contains("authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
                assert!(head.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
                assert!(head.contains("x-amz-content-sha256: "));
                assert!(head.contains("if-none-match: *"));
            },
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        let etag = client
            .put(
                "sst/000001.sst",
                b"hello world",
                Precondition::IfNoneMatchAny,
            )
            .unwrap();
        assert_eq!(etag.as_str(), "\"abc123\"");
        server.join().unwrap();
    }

    #[test]
    fn profile_v1_put_request_is_byte_stable() {
        let config = test_config(1);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.host_header = "objects.test:443".to_string();
        client.with_clock(fixture_clock);
        let payload_hash = HexDigest::of(&sha256(b"hello world"));
        let result = client.send_head_and_connect(
            "PUT",
            "sst/000001.sst",
            "",
            11,
            payload_hash.as_str(),
            Precondition::IfNoneMatchAny,
            None,
        );
        assert!(matches!(result, Err(S3Error::Io { .. })));
        assert_eq!(
            core::str::from_utf8(client.head.readable()).unwrap(),
            concat!(
                "PUT /testbucket/sst/000001.sst HTTP/1.1\r\n",
                "host: objects.test:443\r\n",
                "x-amz-content-sha256: b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9\r\n",
                "x-amz-date: 20150830T123600Z\r\n",
                "if-none-match: *\r\n",
                "content-length: 11\r\n",
                "authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=5a87857b41f9cf593038d7b8992fc853dda4823e39a360d2d5a6765718565c6a\r\n",
                "\r\n"
            )
        );
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
        let got = client
            .get("k", Some(ByteRange::new(10, 14).unwrap()))
            .unwrap();
        assert_eq!(got.len, 5);
        assert_eq!(client.body_bytes(), b"hello");
        server.join().unwrap();
    }

    #[test]
    fn endpoint_is_one_authority_for_tcp_host_and_tls() {
        let ipv4 = Endpoint::parse("objects.example:443").unwrap();
        assert_eq!(ipv4.authority, "objects.example:443");
        assert_eq!(ipv4.tls_host, "objects.example");
        let ipv6 = Endpoint::parse("[2001:db8::1]:9443").unwrap();
        assert_eq!(ipv6.authority, "[2001:db8::1]:9443");
        assert_eq!(ipv6.tls_host, "2001:db8::1");
        for malformed in [
            "https://objects.example:443",
            "objects.example",
            "objects.example:0",
            "objects.example:65536",
            "2001:db8::1:443",
            "[2001:db8::1]",
            "[not-ipv6]:443",
            "bad host:443",
            ".objects.example:443",
        ] {
            assert!(Endpoint::parse(malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn session_token_is_a_header_safe_signed_state() {
        let mut config = test_config(1);
        config.object_store_session_token = "good\r\nbad".to_string();
        let mut budget = Budget::new(1 << 20);
        assert!(matches!(
            S3Client::new(&config, &mut budget),
            Err(S3SetupError::SessionToken)
        ));
    }

    #[test]
    fn virtual_hosted_addressing_moves_only_the_bucket() {
        let mut config = test_config(1);
        config.object_store_endpoint = "localhost:1".to_string();
        config.object_store_addressing = ObjectStoreAddressing::VirtualHosted;
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        assert_eq!(client.host_header, "testbucket.localhost:1");
        client.with_clock(fixture_clock);
        assert!(
            client
                .send_head_and_connect(
                    "GET",
                    "path/key",
                    "",
                    0,
                    EMPTY_SHA256_HEX,
                    Precondition::None,
                    None,
                )
                .is_err()
        );
        assert!(
            core::str::from_utf8(client.head.readable())
                .unwrap()
                .starts_with("GET /path/key HTTP/1.1\r\nhost: testbucket.localhost:1\r\n")
        );

        config.object_store_endpoint = "127.0.0.1:1".to_string();
        let mut budget = Budget::new(1 << 20);
        assert!(matches!(
            S3Client::new(&config, &mut budget),
            Err(S3SetupError::Addressing(_))
        ));
    }

    #[test]
    fn bucket_is_a_valid_s3_path_state() {
        let mut config = test_config(1);
        config.object_store_bucket.clear();
        let mut budget = Budget::new(1 << 20);
        assert!(matches!(
            S3Client::new(&config, &mut budget),
            Err(S3SetupError::Bucket(_))
        ));
        for bucket in ["UPPER", "ab", "-starts", "ends-", "127.0.0.1"] {
            config.object_store_bucket = bucket.to_string();
            let mut budget = Budget::new(1 << 20);
            assert!(matches!(
                S3Client::new(&config, &mut budget),
                Err(S3SetupError::Bucket(_))
            ));
        }
    }

    #[test]
    fn entity_tags_preserve_the_portable_wire_validator() {
        assert_eq!(
            EntityTag::parse("\"opaque-generation\"").unwrap().as_str(),
            "\"opaque-generation\""
        );
        for invalid in [
            "opaque-generation",
            "W/\"weak\"",
            "\"two\"\"tags\"",
            "\"line\nfeed\"",
        ] {
            assert!(EntityTag::parse(invalid).is_err(), "{invalid:?}");
        }
        assert!(ByteRange::new(9, 8).is_err());
    }

    #[test]
    fn response_framing_is_parsed_once_not_inferred_from_defaults() {
        let fixed = parse_head(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n").unwrap();
        assert!(matches!(fixed.framing, BodyFraming::ContentLength(7)));
        let empty = parse_head(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
        assert!(matches!(empty.framing, BodyFraming::Empty));
        for malformed in [
            b"HTTP/1.1 200 OK\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\ncontent-length: 1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\ntransfer-encoding: chunked\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nnot-a-header\r\n\r\n",
        ] {
            assert!(parse_head(malformed).is_err());
        }
    }

    #[test]
    fn standard_xml_errors_are_typed_and_decoded() {
        let error = status_error(
            412,
            b"<Error><Code>PreconditionFailed</Code><Message>a &amp; b</Message></Error>",
        );
        let S3Error::Status {
            code,
            service_code,
            message,
        } = error
        else {
            panic!("wrong error variant")
        };
        assert_eq!(code, 412);
        assert_eq!(service_code.as_str(), "PreconditionFailed");
        assert_eq!(message.as_str(), "a & b");
        assert!(
            !status_error(
                404,
                b"<Error><Code>NoSuchBucket</Code><Message>missing bucket</Message></Error>"
            )
            .is_not_found()
        );
        assert!(
            status_error(
                404,
                b"<Error><Code>NoSuchKey</Code><Message>missing key</Message></Error>"
            )
            .is_not_found()
        );
        assert_eq!(
            decode_xml_text::<32>("page&#38;&#x2F;two")
                .unwrap()
                .as_str(),
            "page&/two"
        );
    }

    #[test]
    fn malformed_list_truncation_is_rejected() {
        let (port, server) = mock_server(
            "HTTP/1.1 200 OK\r\ncontent-length: 69\r\n\r\n<ListBucketResult><IsTruncated>maybe</IsTruncated></ListBucketResult>",
            |head| {
                assert!(head.starts_with(
                    "GET /testbucket?encoding-type=url&list-type=2&prefix= HTTP/1.1\r\n"
                ));
            },
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        assert!(matches!(
            client.list("", |_| {}),
            Err(S3Error::Protocol("invalid IsTruncated value"))
        ));
        server.join().unwrap();
    }

    #[test]
    fn object_key_and_list_prefix_limits_fail_before_io() {
        let config = test_config(1);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        let overlong = "x".repeat(MAX_OBJECT_KEY_BYTES + 1);
        assert!(matches!(
            client.get(&overlong, None),
            Err(S3Error::Protocol("object key exceeds S3 key limit"))
        ));
        assert!(matches!(
            client.list(&overlong, |_| {}),
            Err(S3Error::Protocol("list prefix exceeds S3 key limit"))
        ));
    }

    #[test]
    fn async_get_completes_without_a_second_request() {
        let (port, server) = mock_server(
            "HTTP/1.1 200 OK\r\ncontent-length: 5\r\netag: \"e\"\r\n\r\nhello",
            |_| {},
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.enable_async_gets();
        let mut result = client.get("k", None);
        while matches!(result, Err(S3Error::WouldBlock)) {
            let fd = client.pending_fd().expect("pending GET keeps its socket");
            let mut event = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            assert!(unsafe { libc::poll(&mut event, 1, 1_000) } > 0);
            result = client.advance_pending();
        }
        assert_eq!(result.unwrap().len, 5);
        assert_eq!(client.body_bytes(), b"hello");
        assert!(!client.has_pending());
        server.join().unwrap();
    }

    #[test]
    fn malformed_async_response_clears_pending_connection() {
        let (port, server) = mock_server("NOT-HTTP\r\n\r\n", |_| {});
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.enable_async_gets();
        let mut result = client.get("k", None);
        while matches!(result, Err(S3Error::WouldBlock)) {
            result = client.advance_pending();
        }
        assert!(matches!(result, Err(S3Error::Protocol("not HTTP/1.x"))));
        assert!(!client.has_pending());
        assert!(client.pending_fd().is_none());
        server.join().unwrap();
    }

    #[test]
    fn chunked_body_requires_final_terminator() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"0\r\n").unwrap();
        });
        let stream = TcpStream::connect(address).unwrap();
        let mut transport = tls::Transport::plain(stream);
        let mut budget = Budget::new(4096);
        let mut body = FixedBuf::new(&mut budget, "chunk", 1024).unwrap();
        assert!(matches!(
            read_chunked_body(&mut transport, &[], &mut body),
            Err(S3Error::Io {
                kind: std::io::ErrorKind::UnexpectedEof,
                ..
            })
        ));
        server.join().unwrap();
    }

    #[test]
    fn non_2xx_is_a_status_error() {
        let (port, server) = mock_server(
            "HTTP/1.1 404 Not Found\r\ncontent-length: 70\r\n\r\n<Error><Code>NoSuchKey</Code><Message>object missing</Message></Error>",
            |_| {},
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        let err = client.get("missing", None).unwrap_err();
        assert!(err.is_not_found(), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn list_parses_s3_xml_and_percent_encoded_keys() {
        let xml = "<?xml version=\"1.0\"?><ListBucketResult>\
                   <IsTruncated>false</IsTruncated>\
                   <Contents><Key>wal%2F000001</Key></Contents>\
                   <Contents><Key>wal%2Fspace%20key</Key></Contents>\
                   </ListBucketResult>";
        let respond: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
                xml.len(),
                xml
            )
            .into_boxed_str(),
        );
        let (port, server) =
            mock_server(respond, |head| {
                assert!(head.contains(
                    "GET /testbucket?encoding-type=url&list-type=2&prefix=wal%2F HTTP/1.1"
                ));
            });
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        let mut keys = Vec::new();
        let n = client.list("wal/", |k| keys.push(k.to_string())).unwrap();
        assert_eq!(n, 2);
        assert_eq!(keys, ["wal/000001", "wal/space key"]);
        server.join().unwrap();
    }

    #[test]
    fn list_follows_opaque_continuation_tokens() {
        fn response(xml: &str) -> &'static str {
            Box::leak(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
                    xml.len(),
                    xml
                )
                .into_boxed_str(),
            )
        }
        let first = response(
            "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>p%2Fa</Key></Contents><NextContinuationToken>next&amp;page</NextContinuationToken></ListBucketResult>",
        );
        let second = response(
            "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>p%2Fb</Key></Contents></ListBucketResult>",
        );
        let (port, server) = mock_server_sequence(vec![first, second], |page, head| {
            if page == 0 {
                assert!(head.contains(
                    "GET /testbucket?encoding-type=url&list-type=2&prefix=p%2F HTTP/1.1"
                ));
            } else {
                assert!(head.contains(
                    "GET /testbucket?continuation-token=next%26page&encoding-type=url&list-type=2&prefix=p%2F HTTP/1.1"
                ));
            }
        });
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        let mut keys = Vec::new();
        assert_eq!(
            client.list("p/", |key| keys.push(key.to_string())).unwrap(),
            2
        );
        assert_eq!(keys, ["p/a", "p/b"]);
        server.join().unwrap();
    }

    #[test]
    fn temporary_credentials_are_signed_without_provider_branching() {
        let (port, server) = mock_server(
            "HTTP/1.1 200 OK\r\netag: \"session\"\r\ncontent-length: 0\r\n\r\n",
            |head| {
                assert!(head.contains("x-amz-security-token: temporary-token\r\n"));
                assert!(head.contains(
                    "SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
                ));
            },
        );
        let mut config = test_config(port);
        config.object_store_session_token = "temporary-token".to_string();
        let mut budget = Budget::new(1 << 20);
        let mut client = S3Client::new(&config, &mut budget).unwrap();
        client.put("key", b"value", Precondition::None).unwrap();
        server.join().unwrap();
    }
}
