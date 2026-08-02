//! Server-side TLS for the PostgreSQL wire protocol.
//!
//! rustls is the one whitelisted dependency (TLS is never hand-rolled); this
//! module and [`crate::s3::tls`] are its only doors. The [`rustls::ServerConfig`]
//! is built at startup, before the allocator freezes; every *runtime* rustls
//! call — session construction, the handshake pump, record I/O, teardown — runs
//! inside [`crate::mem::guard::tls_scope`], so its allocations are charged
//! against `tls_pool_bytes` and abort loudly past it.
//!
//! Unlike the blocking S3 client ([`crate::s3::tls`] wraps `StreamOwned`), the
//! server socket is non-blocking and driven by the kqueue reactor, so this uses
//! the low-level `read_tls`/`process_new_packets`/`write_tls` API and translates
//! rustls's read/write wants into the reactor's read/write interest.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::mem::guard;

/// Budget headroom to reserve per concurrent server-side TLS session (rustls
/// record buffers plus handshake state); the `tls_pool_bytes` pool is grown by
/// `max_connections` of these when server TLS is on.
pub const SERVER_SESSION_BYTES: usize = 128 * 1024;

/// Builds the server TLS configuration from a certificate chain and a private
/// key, both PEM files. Startup only (allocation is still free).
pub fn build_server_config(
    cert_file: &str,
    key_file: &str,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let cert_pem = std::fs::read_to_string(cert_file)
        .map_err(|e| format!("tls_cert_file {cert_file}: {e}"))?;
    let certs: Vec<CertificateDer<'static>> = crate::pem::certificates(&cert_pem)?
        .into_iter()
        .map(CertificateDer::from)
        .collect();
    if certs.is_empty() {
        return Err(format!("tls_cert_file {cert_file}: no certificates found"));
    }
    let key_pem =
        std::fs::read_to_string(key_file).map_err(|e| format!("tls_key_file {key_file}: {e}"))?;
    let key = private_key(&key_pem)
        .ok_or_else(|| format!("tls_key_file {key_file}: no private key found"))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("tls certificate/key rejected: {e}"))?;
    Ok(Arc::new(config))
}

/// The first `PRIVATE KEY`/`RSA PRIVATE KEY`/`EC PRIVATE KEY` block; the DER
/// form auto-detects PKCS#8 vs PKCS#1 vs SEC1.
fn private_key(pem: &str) -> Option<PrivateKeyDer<'static>> {
    for block in crate::pem::blocks(pem).ok()? {
        if block.label.contains("PRIVATE KEY")
            && let Ok(key) = PrivateKeyDer::try_from(block.der)
        {
            return Some(key);
        }
    }
    None
}

/// A server-side TLS session over one connection's socket. Every method runs
/// its rustls work inside a `tls_scope`. The session is held in an `Option` so
/// `Drop` can free rustls's buffers *inside* a scope — the memory guard only
/// credits frees that happen in a scope, so dropping it outside would leak the
/// TLS pool.
pub struct ServerSession {
    inner: Option<Box<rustls::ServerConnection>>,
}

impl ServerSession {
    pub fn new(config: &Arc<rustls::ServerConfig>) -> std::io::Result<Self> {
        let inner =
            guard::tls_scope(|| rustls::ServerConnection::new(config.clone()).map(Box::new))
                .map_err(|e| std::io::Error::other(format!("tls session: {e}")))?;
        Ok(Self { inner: Some(inner) })
    }

    fn session(&mut self) -> &mut rustls::ServerConnection {
        self.inner.as_mut().expect("live tls session")
    }

    /// True while the handshake is still in progress.
    pub fn is_handshaking(&self) -> bool {
        self.inner
            .as_ref()
            .expect("live tls session")
            .is_handshaking()
    }

    /// True when rustls has ciphertext queued to send (handshake or records);
    /// the connection must keep write interest registered until it drains.
    pub fn wants_write(&self) -> bool {
        self.inner.as_ref().expect("live tls session").wants_write()
    }

    /// Reads ciphertext off the socket and returns decrypted plaintext into
    /// `dst`. Mirrors `TcpStream::read` semantics for the caller: `Ok(0)` means
    /// the peer closed, `WouldBlock` means no plaintext is ready yet (e.g. still
    /// handshaking), `Ok(n)` delivered `n` plaintext bytes.
    pub fn read(&mut self, socket: &mut TcpStream, dst: &mut [u8]) -> std::io::Result<usize> {
        let session = self.session();
        guard::tls_scope(|| {
            loop {
                match session.read_tls(socket) {
                    Ok(0) => break, // socket EOF; deliver any buffered plaintext below
                    Ok(_) => {
                        session
                            .process_new_packets()
                            .map_err(|e| std::io::Error::other(format!("tls: {e}")))?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            // rustls `Reader`: `Ok(0)` = clean peer close, `WouldBlock` = open
            // but no plaintext ready, `Ok(n)` = data. This matches what the
            // caller expects from a raw socket read.
            session.reader().read(dst)
        })
    }

    /// Buffers plaintext for encryption. Returns how much was accepted.
    pub fn queue(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let session = self.session();
        guard::tls_scope(|| session.writer().write(data))
    }

    /// Flushes queued ciphertext to the (non-blocking) socket, stopping on
    /// `WouldBlock` — the caller keeps write interest until `wants_write` clears.
    pub fn flush_nonblocking(&mut self, socket: &mut TcpStream) -> std::io::Result<()> {
        let session = self.session();
        guard::tls_scope(|| {
            while session.wants_write() {
                match session.write_tls(socket) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
    }

    /// Encrypts and writes all of `data`, blocking until the socket accepts it
    /// (the large-result streaming path puts the socket in blocking mode).
    /// Returns false on error. Records are flushed as they are produced so
    /// rustls never buffers the whole result.
    pub fn write_all_blocking(&mut self, socket: &mut TcpStream, data: &[u8]) -> bool {
        let session = self.session();
        guard::tls_scope(|| {
            let mut written = 0;
            while written < data.len() {
                match session.writer().write(&data[written..]) {
                    Ok(0) => return false,
                    Ok(n) => written += n,
                    Err(_) => return false,
                }
                while session.wants_write() {
                    match session.write_tls(socket) {
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => return false,
                    }
                }
            }
            true
        })
    }
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        // Free rustls's buffers inside a scope so the guard credits the pool
        // (a free outside a scope is not credited, and would leak tls_pool_bytes).
        guard::tls_scope(|| drop(self.inner.take()));
    }
}
