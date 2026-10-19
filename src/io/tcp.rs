//! Nonblocking outbound TCP setup.

use std::net::{SocketAddr, TcpStream};
use std::os::fd::FromRawFd;

/// Starts connecting without waiting. Ownership precedes every fallible setup.
pub(crate) fn connect(address: SocketAddr) -> std::io::Result<TcpStream> {
    let domain = if address.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    let result = match address {
        SocketAddr::V4(address) => {
            let mut raw = unsafe { core::mem::zeroed::<libc::sockaddr_in>() };
            raw.sin_family = libc::AF_INET as libc::sa_family_t;
            raw.sin_port = address.port().to_be();
            raw.sin_addr.s_addr = u32::from_ne_bytes(address.ip().octets());
            unsafe {
                libc::connect(
                    fd,
                    (&raw const raw).cast(),
                    size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(address) => {
            let mut raw = unsafe { core::mem::zeroed::<libc::sockaddr_in6>() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_port = address.port().to_be();
            raw.sin6_addr.s6_addr = address.ip().octets();
            raw.sin6_flowinfo = address.flowinfo();
            raw.sin6_scope_id = address.scope_id();
            unsafe {
                libc::connect(
                    fd,
                    (&raw const raw).cast(),
                    size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EWOULDBLOCK)
        ) {
            return Err(error);
        }
    }
    Ok(stream)
}

/// Zero socket error alone also describes a connection still being established.
pub(crate) fn connected(stream: &TcpStream) -> std::io::Result<bool> {
    if let Some(error) = stream.take_error()? {
        return Err(error);
    }
    match stream.peer_addr() {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOTCONN) | Some(libc::EINPROGRESS)
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}
