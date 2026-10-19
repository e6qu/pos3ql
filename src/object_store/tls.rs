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

/// Keep TLS diagnostics out of heap-backed I/O errors after startup.
pub(crate) fn protocol_error(
    context: &'static str,
    kind: std::io::ErrorKind,
    error: rustls::Error,
) -> std::io::Error {
    guard::tls_scope(|| {
        crate::logging::error_args(context, format_args!("{error}"));
        drop(error);
    });
    std::io::Error::from(kind)
}

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

    pub(crate) fn connected(&self) -> std::io::Result<bool> {
        match self {
            Self::Plain(stream) => {
                crate::io::tcp::connected(stream.as_ref().expect("live plain transport"))
            }
            Self::Tls(session) => {
                crate::io::tcp::connected(&session.as_ref().expect("live session").sock)
            }
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
                        session.conn.process_new_packets().map_err(|error| {
                            protocol_error(
                                "tls_client_records",
                                std::io::ErrorKind::InvalidData,
                                error,
                            )
                        })?;
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
                .map_err(|error| {
                    protocol_error(
                        "tls_client_session",
                        std::io::ErrorKind::InvalidInput,
                        error,
                    )
                })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_object_tls_failed_and_shrinking_buffers_release_budget() {
        const CHILD: &str = "POS3QL_TLS_ALLOCATOR_CASE";
        const MARKER: &[u8] = b"TLS allocator budget recovered";
        if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new("1")) {
            guard::set_tls_budget(u64::MAX);
            guard::tls_scope(|| {
                let impossible = (isize::MAX as usize) / 4;
                let mut existing = Vec::<u8>::with_capacity(128);
                assert!(existing.try_reserve_exact(impossible).is_err());
                let mut empty = Vec::<u8>::new();
                assert!(empty.try_reserve_exact(impossible).is_err());
                let layout = std::alloc::Layout::from_size_align(impossible, 8).unwrap();
                assert!(unsafe { std::alloc::alloc_zeroed(layout) }.is_null());
            });
            guard::set_tls_budget(256);
            for _ in 0..32 {
                guard::forbid_alloc(|| {
                    guard::tls_scope(|| {
                        let mut bytes = Vec::<u8>::with_capacity(128);
                        bytes.resize(64, 0);
                        bytes.shrink_to_fit();
                        drop(std::hint::black_box(bytes));
                    });
                    let error =
                        guard::tls_scope(|| rustls::Error::General("TLS-owned error".to_string()));
                    let error =
                        protocol_error("tls_budget_test", std::io::ErrorKind::InvalidData, error);
                    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                });
            }
            println!("TLS allocator budget recovered");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "object_store::tls::tests::parallel_object_tls_failed_and_shrinking_buffers_release_budget",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success()
                && output
                    .stdout
                    .windows(MARKER.len())
                    .any(|part| part == MARKER),
            "TLS budget subprocess failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn parallel_object_tls_client_setup_error_is_allocation_free() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut config = build_client_config("").unwrap().config;
        Arc::get_mut(&mut config).unwrap().max_fragment_size = Some(1);
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let error = crate::mem::guard::forbid_alloc(|| Transport::tls(stream, &config, &name))
            .err()
            .expect("invalid record size must reject session setup");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
