//! mDNS discovery of Apple TV devices, sans-IO.
//!
//! - [`dns`]: DNS message encoding and parsing.
//! - [`Collector`]: groups records from many responses into [`Device`]s.
//! - [`query`]: builds the query to send to `224.0.0.251:5353`, to
//!   `[ff02::fb]:5353` with a scope id ([`MDNS_IPV6`]), or to a host directly.
//!
//! Send the query from an ephemeral port. Responders then answer by unicast
//! (RFC 6762 section 6.7), so port 5353 does not need to be free.
//!
//! Over IPv6, send to [`MDNS_IPV6`] with the scope id of the interface, for
//! example `SocketAddrV6::new(MDNS_IPV6, MDNS_PORT, 0, if_index)`, or to a
//! link-local IPv6 address of a host with its scope id. Pass the source address
//! of each answer to [`Collector::add`] so link-local addresses get their scope
//! id. An answer from a global address usually gives no scope id.

mod collector;
pub mod dns;
mod txt;

pub use collector::{Address, Collector, Device, Service, ServiceKind};
pub use txt::Txt;

/// mDNS multicast group (IPv4).
pub const MDNS_IPV4: std::net::Ipv4Addr = std::net::Ipv4Addr::new(224, 0, 0, 251);
/// mDNS multicast group (IPv6, link-local scope). Needs the scope id of the interface.
pub const MDNS_IPV6: std::net::Ipv6Addr = std::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
/// mDNS port.
pub const MDNS_PORT: u16 = 5353;

/// Service types queried by default.
pub const DEFAULT_SERVICES: &[ServiceKind] = &[
    ServiceKind::Companion,
    ServiceKind::AirPlay,
    ServiceKind::MediaRemote,
    ServiceKind::Raop,
];

/// Builds a PTR query for the given service types.
#[must_use]
pub fn query(services: &[ServiceKind]) -> Vec<u8> {
    let mut q = dns::Query::new();
    for s in services {
        q.question(s.service_name(), dns::RecordType::PTR);
    }
    q.finish()
}
