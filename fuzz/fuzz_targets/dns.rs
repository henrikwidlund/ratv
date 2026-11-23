//! mDNS response parser and device collector.
#![no_main]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

use libfuzzer_sys::fuzz_target;
use ratv_discovery::{Address, Collector, ServiceKind, dns};

/// Same order as the collector: IPv4, other IPv6, scoped link-local, then
/// link-local without a scope id.
fn rank(a: &Address) -> u8 {
    match a.ip {
        IpAddr::V4(_) => 0,
        IpAddr::V6(ip) if !ip.is_unicast_link_local() => 1,
        IpAddr::V6(_) if a.scope_id != 0 => 2,
        IpAddr::V6(_) => 3,
    }
}

fuzz_target!(|data: &[u8]| {
    let _ = dns::parse(data);
    let ll = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    let link = |scope| Some(SocketAddr::V6(SocketAddrV6::new(ll, 5353, 0, scope)));
    let v4 = Some(SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 5353)));
    // A global address with a scope id: the source address gets scope id 0;
    // link-local AAAA addresses get scope id 3.
    let global = Some(SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
        5353,
        0,
        3,
    )));
    // An IPv4-mapped source with a scope id: stored as IPv4, scope id 0.
    let mapped = Some(SocketAddr::V6(SocketAddrV6::new(
        Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped(),
        5353,
        0,
        3,
    )));
    // Adds a message once from each source in the pair. The pairs with
    // `first` and `second` add the two parts of the input from different
    // sources, so one host can have scoped and unscoped link-local
    // addresses. The first byte selects where the input is split.
    let cut = data.first().map_or(0, |&b| usize::from(b)).min(data.len());
    let (first, second) = data.split_at(cut);
    'pairs: for adds in [
        [(data, None), (data, None)],
        [(data, link(1)), (data, link(2))],
        [(data, v4), (data, link(1))],
        [(data, link(1)), (data, None)],
        [(data, mapped), (data, v4)],
        [(first, v4), (second, link(1))],
        // A scoped link-local source, then an AAAA record for the same IP
        // from an IPv4 packet (no scope id).
        [(first, link(1)), (second, v4)],
        [(data, global), (data, None)],
        // A link-local source without a scope id.
        [(data, link(0)), (data, None)],
    ] {
        let mut c = Collector::new();
        for (msg, source) in adds {
            if c.add(msg, source).is_err() {
                continue 'pairs;
            }
        }
        let _ = (c.follow_up_query(), c.is_complete());
        for device in c.devices().iter().chain(&c.complete_devices()) {
            let _ = (device.name(), device.model(), device.identifier());
            let _ = device.socket_addr(ServiceKind::Companion);
            let addrs = &device.addresses;
            for (i, a) in addrs.iter().enumerate() {
                let _ = (a.socket_addr(1), a.to_string());
                // IPv4-mapped addresses are stored as IPv4.
                assert_eq!(a.ip, a.ip.to_canonical());
                // Only link-local IPv6 addresses have a scope id.
                assert!(
                    a.scope_id == 0
                        || matches!(a.ip, IpAddr::V6(ip) if ip.is_unicast_link_local())
                );
                // No duplicates, and never the same IP with and without a scope id.
                for b in &addrs[i + 1..] {
                    assert!(a != b);
                    assert!(a.ip != b.ip || (a.scope_id != 0 && b.scope_id != 0));
                }
            }
            assert!(addrs.is_sorted_by_key(rank));
        }
    }
});
