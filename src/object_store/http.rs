//! Generic object-store gateway client: hand-rolled HTTP/1.1 over a blocking,
//! keep-alive TCP connection. Plaintext HTTP or TLS uses the isolated
//! [`tls`] door. The gateway, not the database, translates this contract to
//! a concrete durable store.
//!
//! Request heads are assembled in a fixed buffer; bodies are written
//! straight from the caller's slice, so object size is not bounded by any
//! client buffer. Response bodies must fit the fixed response buffer —
//! reads use ranged GETs sized accordingly.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::object_store::{ByteRange, EntityTag, Error, GetResult, Precondition};
use crate::stack_format;
use crate::util::StackStr;

type HttpError = Error;

const MAX_ATTEMPTS: u32 = 3;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum HttpSetupError {
    Budget(BudgetError),
    Endpoint(&'static str),
    Namespace,
    Token,
    Resolve(String, std::io::Error),
    Tls(String),
}

impl std::fmt::Display for HttpSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "{e}"),
            Self::Endpoint(message) => write!(f, "bad object_store_endpoint: {message}"),
            Self::Namespace => write!(f, "bad object_store_namespace: must not be empty"),
            Self::Token => write!(
                f,
                "bad object_store_token: must be visible ASCII without spaces"
            ),
            Self::Resolve(endpoint, e) => {
                write!(f, "cannot resolve object_store_endpoint '{endpoint}': {e}")
            }
            Self::Tls(message) => write!(f, "tls: {message}"),
        }
    }
}

impl std::error::Error for HttpSetupError {}

impl From<BudgetError> for HttpSetupError {
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
            parse_port(port)?;
            return Ok(Self {
                authority,
                tls_host: host,
            });
        }
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or("authority must include a port")?;
        if host.is_empty() || host.contains(':') {
            return Err("IPv6 authority must be bracketed and include a port");
        }
        parse_port(port)?;
        Ok(Self {
            authority,
            tls_host: host,
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

pub struct HttpClient {
    host_header: String,
    /// Resolved once at startup: `TcpStream::connect` on a string would
    /// allocate (ToSocketAddrs builds a Vec), which is forbidden after the
    /// freeze.
    connect_addr: std::net::SocketAddr,
    namespace: String,
    key_prefix: String,
    token: String,
    stream: Option<tls::Transport>,
    /// TLS client state when object-store TLS is on (built at startup).
    tls_context: Option<tls::TlsContext>,
    head: FixedBuf,
    body: FixedBuf,
    /// A non-blocking GET in progress: the response is being read
    /// incrementally, advanced by the reactor when the socket is ready.
    pending: Option<PendingResponse>,
    async_gets: bool,
}

/// Incremental HTTP-response state for a non-blocking GET. The request was
/// sent (blocking write — fast); the response is read in chunks via
/// [`HttpClient::advance_pending`], driven by reactor readability events.
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

impl HttpClient {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        config.object_store_head_bytes + config.object_store_response_bytes
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, HttpSetupError> {
        if config.object_store_namespace.is_empty() {
            return Err(HttpSetupError::Namespace);
        }
        if !config.object_store_token.is_empty()
            && config
                .object_store_token
                .bytes()
                .any(|byte| !(0x21..=0x7e).contains(&byte))
        {
            return Err(HttpSetupError::Token);
        }
        let endpoint =
            Endpoint::parse(&config.object_store_endpoint).map_err(HttpSetupError::Endpoint)?;
        let host_header = endpoint.authority.to_string();
        let connect_addr = {
            use std::net::ToSocketAddrs;
            config
                .object_store_endpoint
                .to_socket_addrs()
                .map_err(|e| HttpSetupError::Resolve(config.object_store_endpoint.clone(), e))?
                .next()
                .ok_or_else(|| {
                    HttpSetupError::Resolve(
                        config.object_store_endpoint.clone(),
                        std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses"),
                    )
                })?
        };
        Ok(Self {
            host_header,
            connect_addr,
            namespace: config.object_store_namespace.clone(),
            key_prefix: config.object_store_prefix.clone(),
            token: config.object_store_token.clone(),
            stream: None,
            tls_context: if config.object_store_tls {
                Some(
                    tls::build_context(endpoint.tls_host, &config.object_store_tls_ca_file)
                        .map_err(HttpSetupError::Tls)?,
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
            pending: None,
            async_gets: false,
        })
    }

    /// Uploads an object. Returns its ETag.
    pub fn put(
        &mut self,
        key: &str,
        body: &[u8],
        precondition: Precondition,
    ) -> Result<EntityTag, HttpError> {
        let result = self.request("PUT", key, "", body, precondition, None)?;
        result
            .etag
            .ok_or(HttpError::Protocol("PUT response missing ETag"))
    }

    /// Downloads an object (or a byte range, inclusive). The bytes are in
    /// [`Self::body_bytes`] afterwards. When a non-blocking GET is in
    /// progress, advances it instead of starting a new request.
    pub fn get(&mut self, key: &str, range: Option<ByteRange>) -> Result<GetResult, HttpError> {
        if self.pending.is_some() {
            return self.advance_pending();
        }
        if !self.async_gets {
            let response = self.request("GET", key, "", &[], Precondition::None, range)?;
            return Ok(GetResult {
                len: response.len,
                etag: response
                    .etag
                    .ok_or(HttpError::Protocol("GET response missing ETag"))?,
            });
        }
        // Initiate: send the request (blocking write — fast), then switch to
        // non-blocking for the response read so the reactor can serve other
        // connections while we wait.
        self.send_head_and_connect("GET", key, "", 0, Precondition::None, range)?;
        let stream = self.stream.as_mut().expect("connected above");
        let send = stream.write_all(&[]).and_then(|()| stream.flush());
        if let Err(e) = send {
            self.stream = None;
            return Err(HttpError::Io {
                context: "send body",
                kind: e.kind(),
                detail: stack_format!(160, "{e}"),
            });
        }
        // Switch to non-blocking for the response read.
        if let Err(e) = self.stream.as_ref().unwrap().set_nonblocking(true) {
            self.stream = None;
            return Err(HttpError::Io {
                context: "set_nonblocking",
                kind: e.kind(),
                detail: stack_format!(160, "{e}"),
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
    pub fn advance_pending(&mut self) -> Result<GetResult, HttpError> {
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
                    return Err(HttpError::Protocol("response head too large"));
                }
                match stream.read(space) {
                    Ok(0) => {
                        self.clear_pending();
                        return Err(HttpError::Io {
                            context: "read head",
                            kind: std::io::ErrorKind::UnexpectedEof,
                            detail: StackStr::new(),
                        });
                    }
                    Ok(n) => self.head.advance(n),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        return Err(HttpError::WouldBlock);
                    }
                    Err(e) => {
                        self.clear_pending();
                        return Err(HttpError::Io {
                            context: "read head",
                            kind: e.kind(),
                            detail: stack_format!(160, "{e}"),
                        });
                    }
                }
            }
        }

        // Phase 2: parse the head (once).
        let head_end = pending.head_end.unwrap();
        let response_head = parse_head(&self.head.readable()[..head_end])?;
        let content_length = match response_head.framing {
            BodyFraming::ContentLength(length) => length,
            BodyFraming::Empty => 0,
            BodyFraming::Chunked => {
                self.clear_pending();
                return Err(HttpError::Protocol(
                    "chunked encoding not supported in non-blocking GET",
                ));
            }
        };
        pending.status = response_head.status;
        pending.content_length = content_length;

        if content_length > self.body.capacity() {
            self.clear_pending();
            return Err(HttpError::ResponseTooLarge {
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
                    return Err(HttpError::Io {
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
                    return Err(HttpError::WouldBlock);
                }
                Err(e) => {
                    self.clear_pending();
                    return Err(HttpError::Io {
                        context: "read body",
                        kind: e.kind(),
                        detail: stack_format!(160, "{e}"),
                    });
                }
            }
        }

        // Phase 4: complete. Deregister from the reactor and restore blocking.
        let _ = self.stream.as_ref().unwrap().set_nonblocking(false);
        self.pending = None;
        if !(200..300).contains(&response_head.status) {
            let text = core::str::from_utf8(self.body.readable()).unwrap_or("");
            return Err(HttpError::Status {
                code: response_head.status,
                message: stack_format!(256, "{}", text),
            });
        }
        Ok(GetResult {
            len: self.body.readable().len(),
            etag: response_head
                .etag
                .ok_or(HttpError::Protocol("GET response missing ETag"))?,
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

    pub fn delete(&mut self, key: &str) -> Result<(), HttpError> {
        match self.request("DELETE", key, "", &[], Precondition::None, None) {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Lists the gateway's newline-delimited logical keys under `prefix`.
    pub fn list(&mut self, prefix: &str, mut each: impl FnMut(&str)) -> Result<usize, HttpError> {
        let mut query = StackStr::<1400>::new();
        use core::fmt::Write;
        let _ = query.write_str("prefix=");
        encode_component(&mut query, &self.key_prefix, false);
        encode_component(&mut query, prefix, false);
        if query.is_truncated() {
            return Err(HttpError::Protocol("list query overflow"));
        }
        self.request("GET", "", query.as_str(), &[], Precondition::None, None)?;
        let keys = core::str::from_utf8(self.body.readable())
            .map_err(|_| HttpError::Protocol("list response is not UTF-8"))?;
        let mut count = 0;
        for key in keys.lines().filter(|key| !key.is_empty()) {
            let logical = key
                .strip_prefix(self.key_prefix.as_str())
                .ok_or(HttpError::Protocol("listed key outside configured prefix"))?;
            each(logical);
            count += 1;
        }
        Ok(count)
    }

    fn request(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        body: &[u8],
        precondition: Precondition,
        range: Option<ByteRange>,
    ) -> Result<Response, HttpError> {
        // Drop any pending non-blocking GET so the connection is clean.
        self.clear_pending();
        let mut last: Option<HttpError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                self.stream = None; // reconnect
                std::thread::sleep(Duration::from_millis(100 << attempt));
            }
            match self.attempt(method, key, query, body, precondition, range) {
                Ok(r) => return Ok(r),
                Err(e @ HttpError::Io { .. }) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.expect("at least one attempt ran"))
    }

    fn attempt(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        body: &[u8],
        precondition: Precondition,
        range: Option<ByteRange>,
    ) -> Result<Response, HttpError> {
        self.send_head_and_connect(method, key, query, body.len() as u64, precondition, range)?;
        let stream = self.stream.as_mut().expect("connected above");
        let send = stream.write_all(body).and_then(|()| stream.flush());
        if let Err(e) = send {
            self.stream = None;
            return Err(HttpError::Io {
                context: "send body",
                kind: e.kind(),
                detail: stack_format!(160, "{e}"),
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

    /// Builds and sends a generic gateway request head.
    fn send_head_and_connect(
        &mut self,
        method: &str,
        key: &str,
        query: &str,
        content_length: u64,
        precondition: Precondition,
        range: Option<ByteRange>,
    ) -> Result<(), HttpError> {
        // Providers are behind the gateway; this path is the only durable
        // storage protocol visible to the database.
        let mut uri = StackStr::<1200>::new();
        {
            use core::fmt::Write;
            let _ = uri.write_str("/v1/objects/");
            encode_component(&mut uri, &self.namespace, false);
            if !key.is_empty() {
                let _ = uri.write_char('/');
                encode_component(&mut uri, &self.key_prefix, true);
                encode_component(&mut uri, key, true);
            }
            if uri.is_truncated() {
                return Err(HttpError::Protocol("key too long"));
            }
        }

        self.head.clear();
        {
            use core::fmt::Write;
            let head = &mut self.head;
            let full = |r: core::fmt::Result| r.map_err(|_| HttpError::Protocol("head overflow"));
            full(write!(head, "{method} {}", uri.as_str()))?;
            if !query.is_empty() {
                full(write!(head, "?{query}"))?;
            }
            full(write!(head, " HTTP/1.1\r\nhost: {}\r\n", self.host_header))?;
            if !self.token.is_empty() {
                full(write!(head, "authorization: Bearer {}\r\n", self.token))?;
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
            full(write!(head, "\r\n"))?;
        }

        // Send.
        let io = |context: &'static str| {
            move |e: std::io::Error| HttpError::Io {
                context,
                kind: e.kind(),
                detail: stack_format!(160, "{e}"),
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
            return Err(HttpError::Io {
                context: "send head",
                kind: e.kind(),
                detail: stack_format!(160, "{e}"),
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
) -> Result<Response, HttpError> {
    // Read until end of head.
    let head_end = loop {
        if let Some(pos) = find_head_end(head.readable()) {
            break pos;
        }
        let space = head.writable();
        if space.is_empty() {
            return Err(HttpError::Protocol("response head too large"));
        }
        let n = stream.read(space).map_err(|e| HttpError::Io {
            context: "read head",
            kind: e.kind(),
            detail: stack_format!(160, "{e}"),
        })?;
        if n == 0 {
            return Err(HttpError::Io {
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
                return Err(HttpError::ResponseTooLarge {
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
                let n = stream.read(&mut space[..want]).map_err(|e| HttpError::Io {
                    context: "read body",
                    kind: e.kind(),
                    detail: stack_format!(160, "{e}"),
                })?;
                if n == 0 {
                    return Err(HttpError::Io {
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
        let text = core::str::from_utf8(body.readable()).unwrap_or("");
        return Err(HttpError::Status {
            code: response_head.status,
            message: stack_format!(256, "{}", text),
        });
    }
    Ok(Response {
        len: body.readable().len(),
        etag: response_head.etag,
    })
}

/// Decodes a `Transfer-Encoding: chunked` body into `body`: hex-sized chunks
/// separated by CRLF, a zero-size chunk ending the stream (trailers, if any,
/// are read to their final CRLF and dropped). The decoded body is still
/// bounded by the response buffer — a loud [`HttpError::ResponseTooLarge`], as
/// for a plain body.
fn read_chunked_body(
    stream: &mut tls::Transport,
    leftover: &[u8],
    body: &mut FixedBuf,
) -> Result<(), HttpError> {
    // Bytes that arrived with the head drain first, then the socket.
    struct Feed<'a> {
        leftover: &'a [u8],
        stream: &'a mut tls::Transport,
    }
    impl Feed<'_> {
        fn read(&mut self, out: &mut [u8]) -> Result<usize, HttpError> {
            if !self.leftover.is_empty() {
                let n = self.leftover.len().min(out.len());
                out[..n].copy_from_slice(&self.leftover[..n]);
                self.leftover = &self.leftover[n..];
                return Ok(n);
            }
            self.stream.read(out).map_err(|e| HttpError::Io {
                context: "read chunk",
                kind: e.kind(),
                detail: stack_format!(160, "{e}"),
            })
        }
    }
    fn fill(
        feed: &mut Feed,
        carry: &mut [u8; 512],
        carry_len: &mut usize,
    ) -> Result<(), HttpError> {
        let n = feed.read(&mut carry[*carry_len..])?;
        if n == 0 {
            return Err(HttpError::Io {
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
                return Err(HttpError::Protocol("chunk size line too long"));
            }
            fill(&mut feed, &mut carry, &mut carry_len)?;
        };
        let line = core::str::from_utf8(&carry[..line_end])
            .map_err(|_| HttpError::Protocol("non-UTF-8 chunk size"))?;
        let hex = line.split(';').next().unwrap_or("").trim();
        let size =
            usize::from_str_radix(hex, 16).map_err(|_| HttpError::Protocol("bad chunk size"))?;
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
                    return Err(HttpError::Protocol("chunk trailers too long"));
                }
                match fill(&mut feed, &mut carry, &mut carry_len) {
                    Ok(()) => {}
                    // Connection close after the last chunk is a valid end.
                    Err(HttpError::Io {
                        kind: std::io::ErrorKind::UnexpectedEof,
                        ..
                    }) if carry_len == 0 => {
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        // Chunk payload: first whatever the carry holds, then the feed.
        let mut remaining = size;
        let from_carry = remaining.min(carry_len);
        if !body.append(&carry[..from_carry]) {
            return Err(HttpError::ResponseTooLarge {
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
                return Err(HttpError::ResponseTooLarge {
                    content_length: body.readable().len() + remaining,
                    capacity: body.capacity(),
                });
            }
            let want = remaining.min(space.len());
            let n = feed.read(&mut space[..want])?;
            if n == 0 {
                return Err(HttpError::Io {
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
            return Err(HttpError::Protocol("chunk missing its trailing CRLF"));
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

fn encode_component<const N: usize>(out: &mut StackStr<N>, input: &str, preserve_slash: bool) {
    use core::fmt::Write;
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (preserve_slash && byte == b'/')
        {
            let _ = out.write_char(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
}

fn parse_head(head: &[u8]) -> Result<ResponseHead, HttpError> {
    let text = core::str::from_utf8(head).map_err(|_| HttpError::Protocol("non-UTF-8 head"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or(HttpError::Protocol("empty response"))?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Protocol("not HTTP/1.x"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or(HttpError::Protocol("bad status"))?;

    let mut content_length = None;
    let mut etag = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpError::Protocol("malformed response header"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(HttpError::Protocol("duplicate content-length header"));
            }
            content_length = Some(
                value
                    .parse()
                    .map_err(|_| HttpError::Protocol("bad content-length"))?,
            );
        } else if name.eq_ignore_ascii_case("etag") {
            if etag.is_some() {
                return Err(HttpError::Protocol("duplicate ETag header"));
            }
            etag = Some(EntityTag::parse(value).map_err(HttpError::Protocol)?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(HttpError::Protocol("unsupported transfer-encoding"));
            }
            chunked = true;
        }
    }
    let framing = match (content_length, chunked) {
        (Some(_), true) => return Err(HttpError::Protocol("response has conflicting framing")),
        (Some(length), false) => BodyFraming::ContentLength(length),
        (None, true) => BodyFraming::Chunked,
        (None, false) if matches!(status, 204 | 304) => BodyFraming::Empty,
        (None, false) => return Err(HttpError::Protocol("response missing body framing")),
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
        c.object_store_namespace = "testnamespace".to_string();
        c.object_store_token = "test-token".to_string();
        c.object_store_head_bytes = 8192;
        c.object_store_response_bytes = 65536;
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
    fn tls_round_trip() {
        // An in-process rustls server (the dependency's own server side — no
        // new dev dependency) answers one canned gateway response over TLS; the
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
        let mut client = HttpClient::new(&config, &mut budget).unwrap();
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
        let mut client = HttpClient::new(&config, &mut budget).unwrap();
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
        let mut client = HttpClient::new(&config, &mut budget).unwrap();
        let err = client.get("k", None).unwrap_err();
        assert!(matches!(err, HttpError::ResponseTooLarge { .. }), "{err:?}");
        // The mock's write may fail once the client stops reading; ignore.
        let _ = server.join();
    }

    #[test]
    fn put_uses_gateway_contract_and_parses_etag() {
        let (port, server) = mock_server(
            "HTTP/1.1 200 OK\r\netag: \"abc123\"\r\ncontent-length: 0\r\n\r\n",
            |head| {
                assert!(
                    head.starts_with("PUT /v1/objects/testnamespace/sst/000001.sst HTTP/1.1\r\n")
                );
                assert!(head.contains("authorization: Bearer test-token\r\n"));
                assert!(head.contains("if-none-match: *"));
            },
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = HttpClient::new(&config, &mut budget).unwrap();
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
    fn get_reads_body_and_range_header() {
        let (port, server) = mock_server(
            "HTTP/1.1 206 Partial Content\r\ncontent-length: 5\r\netag: \"e\"\r\n\r\nhello",
            |head| {
                assert!(head.contains("range: bytes=10-14\r\n"));
            },
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = HttpClient::new(&config, &mut budget).unwrap();
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
        ] {
            assert!(Endpoint::parse(malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn token_is_a_header_safe_startup_state() {
        let mut config = test_config(1);
        config.object_store_token = "good\r\nbad".to_string();
        let mut budget = Budget::new(1 << 20);
        assert!(matches!(
            HttpClient::new(&config, &mut budget),
            Err(HttpSetupError::Token)
        ));
    }

    #[test]
    fn namespace_is_a_nonempty_gateway_path_state() {
        let mut config = test_config(1);
        config.object_store_namespace.clear();
        let mut budget = Budget::new(1 << 20);
        assert!(matches!(
            HttpClient::new(&config, &mut budget),
            Err(HttpSetupError::Namespace)
        ));
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
    fn async_get_completes_without_a_second_request() {
        let (port, server) = mock_server(
            "HTTP/1.1 200 OK\r\ncontent-length: 5\r\netag: \"e\"\r\n\r\nhello",
            |_| {},
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = HttpClient::new(&config, &mut budget).unwrap();
        client.enable_async_gets();
        let mut result = client.get("k", None);
        while matches!(result, Err(HttpError::WouldBlock)) {
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
    fn non_2xx_is_a_status_error() {
        let (port, server) = mock_server(
            "HTTP/1.1 404 Not Found\r\ncontent-length: 24\r\n\r\n<Error>NoSuchKey</Error>",
            |_| {},
        );
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = HttpClient::new(&config, &mut budget).unwrap();
        let err = client.get("missing", None).unwrap_err();
        assert!(err.is_not_found(), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn list_parses_gateway_keys() {
        let xml = "wal/000001\nwal/000002\n";
        let respond: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
                xml.len(),
                xml
            )
            .into_boxed_str(),
        );
        let (port, server) = mock_server(respond, |head| {
            assert!(head.contains("GET /v1/objects/testnamespace?prefix=wal%2F HTTP/1.1"));
        });
        let config = test_config(port);
        let mut budget = Budget::new(1 << 20);
        let mut client = HttpClient::new(&config, &mut budget).unwrap();
        let mut keys = Vec::new();
        let n = client.list("wal/", |k| keys.push(k.to_string())).unwrap();
        assert_eq!(n, 2);
        assert_eq!(keys, ["wal/000001", "wal/000002"]);
        server.join().unwrap();
    }
}
