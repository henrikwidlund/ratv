//! mDNS discovery of Apple TV devices, sans-IO.
//!
//! - [`dns`]: DNS message encoding and parsing.
//! - [`Collector`]: groups records from many responses into [`Device`]s.
//! - [`query`]: builds the query to send to `224.0.0.251:5353` (or a host directly).
//!
//! Send the query from an ephemeral port. Responders then answer by unicast
//! (RFC 6762 section 6.7), so port 5353 does not need to be free.

mod collector;
pub mod dns;
mod txt;

pub use collector::{Collector, Device, Service, ServiceKind};
pub use txt::Txt;

/// mDNS multicast group (IPv4).
pub const MDNS_IPV4: std::net::Ipv4Addr = std::net::Ipv4Addr::new(224, 0, 0, 251);
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
