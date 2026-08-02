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

/// A connection to the object store: plaintext, or TLS over the same socket.
pub enum Transport {
    Plain(TcpStream),
    Tls(Option<Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>>),
}

impl Transport {
    pub(crate) fn plain(stream: TcpStream) -> Self {
        Transport::Plain(stream)
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        let stream = match self {
            Transport::Plain(s) => s,
            Transport::Tls(t) => &t.as_ref().expect("live session").sock,
        };
        stream.set_nonblocking(nonblocking)
    }

    pub(crate) fn raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Transport::Plain(s) => s.as_raw_fd(),
            Transport::Tls(t) => t.as_ref().expect("live session").sock.as_raw_fd(),
        }
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

/// The startup-built client state for TLS endpoints: `None` when object-store TLS
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
            .map_err(|e| format!("object_store_tls_ca_file {ca_file}: {e}"))?;
        let mut added = 0usize;
        for der in crate::pem::certificates(&pem)? {
            roots
                .add(rustls::pki_types::CertificateDer::from(der))
                .map_err(|e| {
                    format!("object_store_tls_ca_file {ca_file}: bad certificate: {e}")
                })?;
            added += 1;
        }
        if added == 0 {
            return Err(format!(
                "object_store_tls_ca_file {ca_file}: no certificates found"
            ));
        }
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("object_store_endpoint host {host}: {e}"))?;
    Ok(TlsContext { config: Arc::new(config), server_name })
}
