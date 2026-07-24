//! The isolated TLS component. rustls is the single whitelisted exception to
//! the libc-only dependency policy (TLS is never hand-rolled — that would be
//! irresponsible), and this module is its only door: the client
//! configuration is built at startup, before the allocator freezes, and every
//! runtime call — handshakes, record I/O, session teardown — enters through
//! [`crate::mem::guard::tls_scope`], whose allocations are charged against
//! the `tls_pool_bytes` budget and abort loudly past it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use crate::mem::guard;

/// A connection to the object store: plaintext, or TLS over the same socket.
pub enum Transport {
    Plain(TcpStream),
    Tls(Option<Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>>),
}

impl Transport {
    pub(crate) fn plain(stream: TcpStream) -> Self {
        Transport::Plain(stream)
    }

    pub(crate) fn tls(
        stream: TcpStream,
        config: &Arc<rustls::ClientConfig>,
        server_name: &rustls::pki_types::ServerName<'static>,
    ) -> std::io::Result<Self> {
        let session = guard::tls_scope(|| {
            rustls::ClientConnection::new(config.clone(), server_name.clone())
        })
        .map_err(|e| std::io::Error::other(format!("tls session: {e}")))?;
        Ok(Transport::Tls(Some(guard::tls_scope(|| {
            Box::new(rustls::StreamOwned::new(session, stream))
        }))))
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(t) => {
                guard::tls_scope(|| t.as_mut().expect("live session").read(buf))
            }
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(t) => {
                guard::tls_scope(|| t.as_mut().expect("live session").write(buf))
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(t) => {
                guard::tls_scope(|| t.as_mut().expect("live session").flush())
            }
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // The session's teardown frees rustls buffers — that too runs inside
        // a scope, so the pool accounting credits the bytes back.
        if let Transport::Tls(t) = self {
            guard::tls_scope(|| drop(t.take()));
        }
    }
}

/// The startup-built client state for TLS endpoints: `None` when `s3_tls`
/// is off.
pub(super) struct TlsContext {
    pub config: Arc<rustls::ClientConfig>,
    pub server_name: rustls::pki_types::ServerName<'static>,
}

/// Builds the TLS client configuration at startup (allocation is still free
/// then): Mozilla's compiled-in roots plus, when `ca_file` names a PEM, the
/// certificates it holds — the door for self-signed test endpoints, decided
/// explicitly in configuration rather than by an insecure-skip flag.
pub(super) fn build_context(
    host: &str,
    ca_file: &str,
) -> Result<TlsContext, String> {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if !ca_file.is_empty() {
        let pem = std::fs::read_to_string(ca_file)
            .map_err(|e| format!("s3_tls_ca_file {ca_file}: {e}"))?;
        let mut added = 0usize;
        for der in pem_certificates(&pem)? {
            roots
                .add(rustls::pki_types::CertificateDer::from(der))
                .map_err(|e| format!("s3_tls_ca_file {ca_file}: bad certificate: {e}"))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!("s3_tls_ca_file {ca_file}: no certificates found"));
        }
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("s3_endpoint host {host}: {e}"))?;
    Ok(TlsContext { config: Arc::new(config), server_name })
}

/// Extracts the DER payloads of every `-----BEGIN CERTIFICATE-----` block in
/// a PEM file, decoding the base64 by hand (the codebase already refuses a
/// base64 dependency; startup-only, allocation still free).
fn pem_certificates(pem: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut b64 = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line == "-----BEGIN CERTIFICATE-----" {
            in_block = true;
            b64.clear();
        } else if line == "-----END CERTIFICATE-----" {
            in_block = false;
            out.push(base64_decode(&b64)?);
        } else if in_block {
            b64.push_str(line);
        }
    }
    Ok(out)
}

fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    const BAD: u8 = 0xFF;
    fn value(c: u8) -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => BAD,
        }
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in text.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = value(c);
        if v == BAD {
            return Err("invalid base64 in certificate".to_string());
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}
