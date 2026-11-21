//! Explicit listener binds for GitTerm's local servers (task MCP, browser
//! MCP, log server). Production keeps each server's own V6 port band; a
//! preview host binds an OS-assigned loopback port that never lands in one.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::ops::RangeInclusive;

/// Every port V6 production reserves: log server 33030–34029, browser MCP
/// 34030–35029, task MCP 35030–36029 (100 instance slots of 10 each).
pub const V6_PORT_BANDS: RangeInclusive<u16> = 33_030..=36_029;

/// OS-assigned ports drawn before giving up. Linux hands out ephemeral
/// ports from 32768 upwards, which covers the V6 bands.
const EPHEMERAL_ATTEMPTS: usize = 64;

/// Where a server listens, chosen by the host instead of the V6 band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerBind {
    /// Exactly this address. It must be a loopback address: the log server
    /// has no authentication.
    Addr(SocketAddr),
    /// 127.0.0.1 with an OS-assigned port outside [`V6_PORT_BANDS`].
    LoopbackEphemeral,
}

impl ListenerBind {
    pub fn bind(self) -> io::Result<TcpListener> {
        match self {
            Self::Addr(addr) if !addr.ip().is_loopback() => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to bind a GitTerm server to non-loopback address {addr}"),
            )),
            Self::Addr(addr) => TcpListener::bind(addr),
            Self::LoopbackEphemeral => bind_ephemeral_outside(&V6_PORT_BANDS),
        }
    }
}

fn bind_ephemeral_outside(reserved: &RangeInclusive<u16>) -> io::Result<TcpListener> {
    // Rejected listeners stay bound until we return, so each new request
    // gets a different port.
    let mut rejected = Vec::new();
    for _ in 0..EPHEMERAL_ATTEMPTS {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        if !reserved.contains(&listener.local_addr()?.port()) {
            return Ok(listener);
        }
        rejected.push(listener);
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        format!(
            "no OS-assigned loopback port outside {}..={} after {EPHEMERAL_ATTEMPTS} attempts",
            reserved.start(),
            reserved.end()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn ephemeral_bind_is_loopback_and_outside_v6_bands() {
        let listener = ListenerBind::LoopbackEphemeral.bind().unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
        assert!(!V6_PORT_BANDS.contains(&addr.port()));
    }

    #[test]
    fn ephemeral_bind_fails_rather_than_return_a_reserved_port() {
        let error = bind_ephemeral_outside(&(0..=u16::MAX)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
    }

    #[test]
    fn explicit_addr_must_be_loopback() {
        let error = ListenerBind::Addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .bind()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            ListenerBind::Addr(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)))
                .bind()
                .is_err()
        );
        let listener = ListenerBind::Addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .bind()
            .unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }
}
