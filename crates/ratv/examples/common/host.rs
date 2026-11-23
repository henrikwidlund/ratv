//! Reads a host address from the command line.

use std::net::{IpAddr, SocketAddr, SocketAddrV6};

use ratv::discovery::MDNS_PORT;

/// Parses `ip` or, for IPv6, `ip%scope`, where `scope` is the interface
/// index, for example `fe80::1%4`. Returns the mDNS address of the host, or
/// `None` if `host` is not in this form.
pub(crate) fn mdns_addr(host: &str) -> Option<SocketAddr> {
    let (ip, scope) = match host.split_once('%') {
        Some((ip, scope)) => (ip, Some(scope.parse().ok()?)),
        None => (host, None),
    };
    let ip: IpAddr = ip.parse().ok()?;
    // IPv4-mapped input becomes IPv4, so the examples keep the default IPv4
    // socket.
    let ip = if scope.is_none() {
        ip.to_canonical()
    } else {
        ip
    };
    match (ip, scope) {
        (IpAddr::V6(ip), scope) => Some(SocketAddr::V6(SocketAddrV6::new(
            ip,
            MDNS_PORT,
            0,
            scope.unwrap_or(0),
        ))),
        (ip, None) => Some(SocketAddr::new(ip, MDNS_PORT)),
        (IpAddr::V4(_), Some(_)) => None,
    }
}
