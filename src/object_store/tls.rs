//! The isolated TLS component. rustls is the single whitelisted exception to
//! the libc-only dependency policy (TLS is never hand-rolled — that would be
//! irresponsible), and this module is its only door: the client
//! configuration is built at startup, before the allocator freezes, and every
//! runtime call — handshakes, record I/O, session teardown — enters through
//! [`crate::mem::guard::tls_scope`], whose allocations are charged against
//! the `tls_pool_bytes` budget and abort loudly past it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use crate::mem::guard;

/// TLS-pool headroom for one non-blocking outbound PostgreSQL client.  The
/// subscription count is fixed at startup, so `main` reserves this for every
/// possible worker before freezing allocation.
pub(crate) const CLIENT_SESSION_BYTES: usize = 128 * 1024;

/// A client connection: plaintext, or TLS over the same socket.
pub(crate) enum Transport {
    Plain(Option<TcpStream>),
    Tls(Option<Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>>),
}

impl Transport {
    pub(crate) fn plain(stream: TcpStream) -> Self {
        Transport::Plain(Some(stream))
    }

    pub(crate) fn into_plain(self) -> TcpStream {
        let mut transport = self;
        match &mut transport {
            Self::Plain(stream) => stream.take().expect("live plain transport"),
            Self::Tls(_) => panic!("attempted to extract a TLS transport as plaintext"),
        }
    }

    pub(crate) fn raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Transport::Plain(s) => s.as_ref().expect("live plain transport").as_raw_fd(),
            Transport::Tls(t) => t.as_ref().expect("live session").sock.as_raw_fd(),
        }
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream
                .as_ref()
                .expect("live plain transport")
                .set_nonblocking(nonblocking),
            Self::Tls(session) => session
                .as_ref()
                .expect("live session")
                .sock
                .set_nonblocking(nonblocking),
        }
    }

    /// Whether a non-blocking caller must retain write readiness.  TLS can
    /// produce handshake or acknowledgement records while processing a read.
    pub(crate) fn wants_write(&self) -> bool {
        match self {
            Self::Plain(_) => false,
            Self::Tls(session) => session.as_ref().expect("live session").conn.wants_write(),
        }
    }

    /// Processes one non-blocking ciphertext read and returns only decrypted
    /// application bytes.  One read preserves already-decoded records when a
    /// peer closes its socket without a TLS close notification; the caller can
    /// finish those protocol frames before observing the close on its next
    /// readiness event.
    pub(crate) fn read_nonblocking(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.as_mut().expect("live plain transport").read(dst),
            Self::Tls(session) => guard::tls_scope(|| {
                let session = session.as_mut().expect("live session");
                match session.conn.read_tls(&mut session.sock) {
                    Ok(0) => {}
                    Ok(_) => {
                        session
                            .conn
                            .process_new_packets()
                            .map_err(|error| std::io::Error::other(format!("tls: {error}")))?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                }
                session.conn.reader().read(dst)
            }),
        }
    }

    /// Queues application data without attempting blocking transport I/O.
    pub(crate) fn queue_nonblocking(&mut self, src: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.as_mut().expect("live plain transport").write(src),
            Self::Tls(session) => guard::tls_scope(|| {
                session
                    .as_mut()
                    .expect("live session")
                    .conn
                    .writer()
                    .write(src)
            }),
        }
    }

    /// Drains queued TLS ciphertext until the socket would block.  Plaintext
    /// has no internal record queue, so its write queue stays in the caller.
    pub(crate) fn flush_nonblocking(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(_) => Ok(()),
            Self::Tls(session) => guard::tls_scope(|| {
                let session = session.as_mut().expect("live session");
                while session.conn.wants_write() {
                    match session.conn.write_tls(&mut session.sock) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            }),
        }
    }

    pub(crate) fn tls(
        stream: TcpStream,
        config: &Arc<rustls::ClientConfig>,
        server_name: &rustls::pki_types::ServerName<'static>,
    ) -> std::io::Result<Self> {
        let session =
            guard::tls_scope(|| rustls::ClientConnection::new(config.clone(), server_name.clone()))
                .map_err(|e| std::io::Error::other(format!("tls session: {e}")))?;
        Ok(Transport::Tls(Some(guard::tls_scope(|| {
            Box::new(rustls::StreamOwned::new(session, stream))
        }))))
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.as_mut().expect("live plain transport").read(buf),
            Transport::Tls(t) => guard::tls_scope(|| t.as_mut().expect("live session").read(buf)),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.as_mut().expect("live plain transport").write(buf),
            Transport::Tls(t) => guard::tls_scope(|| t.as_mut().expect("live session").write(buf)),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.as_mut().expect("live plain transport").flush(),
            Transport::Tls(t) => guard::tls_scope(|| t.as_mut().expect("live session").flush()),
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

/// The startup-built client state for TLS endpoints: `None` when object-store TLS
/// is off.
pub(crate) struct TlsContext {
    pub config: Arc<rustls::ClientConfig>,
    pub server_name: rustls::pki_types::ServerName<'static>,
}

/// Startup-built roots and cipher configuration reusable by any outbound
/// protocol.  The endpoint identity remains protocol-owned, so no provider
/// setting or SDK leaks above a generic TLS boundary.
pub(crate) struct ClientTlsConfig {
    pub config: Arc<rustls::ClientConfig>,
}

pub(crate) fn build_client_config(ca_file: &str) -> Result<ClientTlsConfig, String> {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if !ca_file.is_empty() {
        let pem =
            std::fs::read_to_string(ca_file).map_err(|e| format!("TLS CA file {ca_file}: {e}"))?;
        let mut added = 0usize;
        for der in crate::pem::certificates(&pem)? {
            roots
                .add(rustls::pki_types::CertificateDer::from(der))
                .map_err(|e| format!("TLS CA file {ca_file}: bad certificate: {e}"))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!("TLS CA file {ca_file}: no certificates found"));
        }
    }
    Ok(ClientTlsConfig {
        config: Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        ),
    })
}

/// Builds the TLS client configuration at startup (allocation is still free
/// then): Mozilla's compiled-in roots plus, when `ca_file` names a PEM, the
/// certificates it holds — the door for self-signed test endpoints, decided
/// explicitly in configuration rather than by an insecure-skip flag.
pub(crate) fn build_context(host: &str, ca_file: &str) -> Result<TlsContext, String> {
    let config = build_client_config(ca_file)?.config;
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("object_store_endpoint host {host}: {e}"))?;
    Ok(TlsContext {
        config,
        server_name,
    })
}
