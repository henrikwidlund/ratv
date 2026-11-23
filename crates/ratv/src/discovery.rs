//! Find Apple TV devices with mDNS.
//!
//! ```no_run
//! # async fn run() -> Result<(), ratv::Error> {
//! use ratv::discovery::{ScanOptions, ServiceKind, scan};
//!
//! for device in scan(&ScanOptions::default()).await? {
//!     println!("{} {:?} {:?}", device.name(), device.model(), device.socket_addr(ServiceKind::Companion));
//! }
//! # Ok(())
//! # }
//! ```

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

pub use ratv_discovery::{
    Address, DEFAULT_SERVICES, Device, MDNS_IPV4, MDNS_IPV6, MDNS_PORT, Service, ServiceKind, Txt,
};
use ratv_discovery::{Collector, query};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::Error;

const RESEND_INTERVAL: Duration = Duration::from_secs(1);

/// Options for [`scan`].
///
/// Start from `ScanOptions::default()` and change the fields you need.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ScanOptions {
    /// Total scan time.
    pub timeout: Duration,
    /// Where to send queries. Default is the mDNS multicast group. Use
    /// `SocketAddr::new(ip, MDNS_PORT)` to ask known hosts directly when
    /// multicast is blocked. If all targets are unicast, the scan ends when
    /// every target has answered with complete records. A target that cannot
    /// be sent to, for example `fe80::` without a scope id, keeps the scan
    /// open until the timeout.
    pub targets: Vec<SocketAddr>,
    /// Local address to send from; selects the network interface. Default is
    /// `0.0.0.0:0`. Keep port 0, which picks a free port: on port 5353,
    /// devices answer by multicast, which this socket does not get. For a
    /// link-local IPv6 address, set the scope id of the interface. The IPv6
    /// address `[::]` also sends to IPv4 targets, if the system allows IPv4
    /// on IPv6 sockets.
    pub bind: SocketAddr,
    /// Service types to look for.
    pub services: Vec<ServiceKind>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
            targets: vec![SocketAddr::new(IpAddr::V4(MDNS_IPV4), MDNS_PORT)],
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            services: DEFAULT_SERVICES.to_vec(),
        }
    }
}

/// Scans the network and returns the devices that answered.
///
/// A link-local IPv6 address can only be used with a scope id. The scope id is
/// known only when the answer came over IPv6. For that, bind to an IPv6 address
/// (for example `[::]:0`) and use IPv6 targets: [`MDNS_IPV6`] with the scope id
/// of the interface, or a link-local IPv6 address of the TV with its scope id.
/// A global IPv6 target works too, but it usually gives no scope id for
/// link-local addresses in the answer. See [`Address::scope_id`].
///
/// # Errors
///
/// [`Error::Io`] if the socket cannot be bound, or if every send failed
/// (for example when the network is down). The error is the last send error
/// and comes when the timeout ends. If at least one query was sent and nobody
/// answered, or the timeout is zero, returns an empty list. With no targets,
/// returns an empty list at once; the socket is not bound.
pub async fn scan(options: &ScanOptions) -> Result<Vec<Device>, Error> {
    run(options, None::<fn(&Device) -> bool>).await
}

/// Like [`scan`], but ends as soon as `stop` returns true for a device.
/// Returns all devices found until then.
///
/// `stop` runs after each valid packet, so it sees the same device many
/// times. It sees only services with all records. A service that has not
/// answered yet is missing, so check the services you need.
///
/// # Errors
///
/// Same as [`scan`]. So with a very long timeout and no network, this does
/// not return.
pub async fn scan_until(
    options: &ScanOptions,
    stop: impl FnMut(&Device) -> bool,
) -> Result<Vec<Device>, Error> {
    run(options, Some(stop)).await
}

async fn run(
    options: &ScanOptions,
    mut stop: Option<impl FnMut(&Device) -> bool>,
) -> Result<Vec<Device>, Error> {
    if options.targets.is_empty() {
        return Ok(Vec::new());
    }
    let socket = bind(options.bind)?;
    let initial = query(&options.services);
    let start = Instant::now();
    // A very long timeout, for example `Duration::MAX` with `scan_until`,
    // would overflow.
    let deadline = start
        .checked_add(options.timeout)
        .unwrap_or_else(|| start + Duration::from_hours(100 * 365 * 24));
    let mut collector = Collector::new();
    let mut buf = vec![0u8; 9000];
    let mut next_send = Instant::now();
    let unicast = options
        .targets
        .iter()
        .all(|t| !t.ip().to_canonical().is_multicast());
    let mut answered = vec![false; options.targets.len()];
    let mut sent = false;
    let mut send_err = None;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_send {
            // Ask again for anything still missing, otherwise repeat the first query.
            let follow = collector.follow_up_query();
            let msg = follow.as_deref().unwrap_or(&initial);
            for &target in &options.targets {
                match socket.send_to(msg, send_addr(options.bind, target)).await {
                    Ok(_) => sent = true,
                    Err(err) => {
                        tracing::debug!(%err, %target, "mDNS send failed");
                        send_err = Some(err);
                    }
                }
            }
            next_send = now + RESEND_INTERVAL;
        }
        let wait = deadline.min(next_send);
        match tokio::time::timeout_at(wait, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if let Err(err) = collector.add(&buf[..n], Some(from)) {
                    tracing::debug!(%err, %from, "invalid mDNS response");
                    continue;
                }
                tracing::trace!(%from, len = n, complete = collector.is_complete(), "mDNS response");
                if let Some(stop) = &mut stop
                    && collector.complete_devices().iter().any(&mut *stop)
                {
                    break;
                }
                if unicast
                    && unicast_answer(
                        &mut answered,
                        &options.targets,
                        from.ip().to_canonical(),
                        collector.is_complete(),
                        &mut next_send,
                    )
                {
                    break;
                }
            }
            Ok(Err(err)) => tracing::debug!(%err, "mDNS receive failed"),
            Err(_) => {}
        }
    }
    if !sent && let Some(err) = send_err {
        return Err(err.into());
    }
    let devices = collector.devices();
    tracing::debug!(
        devices = devices.len(),
        elapsed = ?start.elapsed(),
        "scan done"
    );
    Ok(devices)
}

/// Binds the UDP socket. An IPv6 socket bound to `[::]` also sends to IPv4
/// targets, if the system allows it.
fn bind(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    if addr.is_ipv4() {
        let socket = std::net::UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        return UdpSocket::from_std(socket);
    }
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    // Windows allows only IPv6 on IPv6 sockets by default. Some systems do
    // not allow IPv4 on IPv6 sockets; IPv4 targets then fail when sent.
    let _ = socket.set_only_v6(false);
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

/// `target` as the socket needs it: IPv4 as IPv4-mapped on an IPv6 socket,
/// IPv4-mapped as IPv4 on an IPv4 socket.
fn send_addr(bind: SocketAddr, target: SocketAddr) -> SocketAddr {
    match (bind, target) {
        (SocketAddr::V6(_), SocketAddr::V4(v4)) => {
            SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port())
        }
        (SocketAddr::V4(_), SocketAddr::V6(v6)) if let Some(v4) = v6.ip().to_ipv4_mapped() => {
            SocketAddr::new(v4.into(), v6.port())
        }
        _ => target,
    }
}

/// Marks the targets at `from` as answered. Returns true when all records
/// are complete and all targets answered. If records are missing, sets
/// `next_send` to now to ask for them at once.
fn unicast_answer(
    answered: &mut [bool],
    targets: &[SocketAddr],
    from: IpAddr,
    complete: bool,
    next_send: &mut Instant,
) -> bool {
    for (done, target) in answered.iter_mut().zip(targets) {
        *done |= target.ip().to_canonical() == from;
    }
    if !complete {
        // Ask for missing records now instead of after the resend interval.
        *next_send = Instant::now();
        return false;
    }
    answered.iter().all(|&a| a)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use ratv_discovery::dns::Response;

    use super::*;

    /// An IPv6 socket also takes IPv4, so IPv4 targets can be sent to. Only
    /// Windows makes IPv6 sockets IPv6-only by default.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn ipv6_socket_takes_ipv4() {
        let Ok(socket) = bind("[::1]:0".parse().unwrap()) else {
            eprintln!("skipped: no IPv6 on this host");
            return;
        };
        assert!(!socket2::SockRef::from(&socket).only_v6().unwrap());
    }

    #[tokio::test]
    async fn unicast_scan_against_fake_responder() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = responder.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            let mut first = true;
            loop {
                let (_, from) = responder.recv_from(&mut buf).await.unwrap();
                let mut r = Response::new();
                let inst = "Den._companion-link._tcp.local";
                if first {
                    // Only the PTR first; the scanner must ask for the rest.
                    r.ptr("_companion-link._tcp.local", inst);
                    first = false;
                } else {
                    r.srv(inst, 49153, "den.local")
                        .txt(inst, &["rpMd=AppleTV14,1"]);
                }
                responder.send_to(&r.finish(), from).await.unwrap();
                responder.send_to(b"bad", from).await.unwrap();
            }
        });
        let options = ScanOptions {
            timeout: Duration::from_millis(1500),
            targets: vec![addr],
            bind: (Ipv4Addr::LOCALHOST, 0).into(),
            ..ScanOptions::default()
        };
        let start = Instant::now();
        let devices = scan(&options).await.unwrap();
        // Ends when the records are complete, not after the timeout.
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name(), "Den");
        assert_eq!(
            devices[0].addresses,
            [IpAddr::V4(Ipv4Addr::LOCALHOST).into()]
        );
        assert_eq!(
            devices[0].service(ServiceKind::Companion).unwrap().port,
            49153
        );
    }

    #[tokio::test]
    async fn scan_until_ends_early() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = responder.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let (_, from) = responder.recv_from(&mut buf).await.unwrap();
                let mut r = Response::new();
                let inst = "Den._companion-link._tcp.local";
                r.ptr("_companion-link._tcp.local", inst)
                    .srv(inst, 49153, "den.local")
                    .txt(inst, &["rpMRtID=ABC"])
                    // Never complete, so a plain scan waits for the timeout.
                    .ptr("_airplay._tcp.local", "Den._airplay._tcp.local");
                responder.send_to(&r.finish(), from).await.unwrap();
            }
        });
        let options = ScanOptions {
            timeout: Duration::from_millis(1500),
            targets: vec![addr],
            bind: (Ipv4Addr::LOCALHOST, 0).into(),
            ..ScanOptions::default()
        };
        // A timeout too large for `Instant` must not panic.
        let endless = ScanOptions {
            timeout: Duration::MAX,
            ..options.clone()
        };
        let start = Instant::now();
        let mut seen = 0;
        let scan = scan_until(&endless, |d| {
            seen += 1;
            d.identifier() == Some("ABC")
        });
        let devices = tokio::time::timeout(Duration::from_secs(5), scan)
            .await
            .expect("scan_until did not stop")
            .unwrap();
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(seen, 1);
        assert_eq!(devices.len(), 1);

        let start = Instant::now();
        let devices = scan_until(&options, |_| false).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(1500));
        assert_eq!(devices.len(), 1);
    }

    #[tokio::test]
    async fn no_targets_returns_at_once() {
        let options = ScanOptions {
            timeout: Duration::from_secs(5),
            targets: Vec::new(),
            bind: (Ipv4Addr::LOCALHOST, 0).into(),
            ..ScanOptions::default()
        };
        let start = Instant::now();
        assert_eq!(scan(&options).await.unwrap(), []);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn all_sends_fail() {
        // An IPv4 socket cannot send to an IPv6 address.
        let bad = SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), MDNS_PORT);
        let mut options = ScanOptions {
            timeout: Duration::from_millis(100),
            targets: vec![bad],
            bind: (Ipv4Addr::LOCALHOST, 0).into(),
            ..ScanOptions::default()
        };
        assert!(matches!(scan(&options).await, Err(Error::Io(_))));

        // One send works, nobody answers: no error.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        options.targets.push(silent.local_addr().unwrap());
        assert_eq!(scan(&options).await.unwrap(), []);
    }

    #[test]
    fn send_addr_maps_ipv4() {
        let v4: SocketAddr = "10.0.0.5:5353".parse().unwrap();
        let v6: SocketAddr = "[fe80::1]:5353".parse().unwrap();
        let any6: SocketAddr = "[::]:0".parse().unwrap();
        let any4: SocketAddr = "0.0.0.0:0".parse().unwrap();
        assert_eq!(
            send_addr(any6, v4),
            "[::ffff:10.0.0.5]:5353".parse().unwrap()
        );
        assert_eq!(send_addr(any6, v6), v6);
        assert_eq!(send_addr(any4, v4), v4);
        let mapped: SocketAddr = "[::ffff:10.0.0.5]:5353".parse().unwrap();
        assert_eq!(send_addr(any4, mapped), v4);
    }

    #[test]
    fn mapped_target_is_answered() {
        let target = "[::ffff:10.0.0.5]:5353".parse().unwrap();
        let mut answered = [false];
        let mut next = Instant::now();
        let from = IpAddr::from([10, 0, 0, 5]);
        assert!(unicast_answer(
            &mut answered,
            &[target],
            from,
            true,
            &mut next
        ));
    }

    /// An IPv6 socket sends to an IPv4 target.
    #[tokio::test]
    async fn ipv6_socket_ipv4_target() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = responder.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let (_, from) = responder.recv_from(&mut buf).await.unwrap();
                let mut r = Response::new();
                let inst = "Den._companion-link._tcp.local";
                r.ptr("_companion-link._tcp.local", inst)
                    .srv(inst, 49153, "den.local")
                    .txt(inst, &[]);
                responder.send_to(&r.finish(), from).await.unwrap();
            }
        });
        let bind = SocketAddr::new(Ipv4Addr::LOCALHOST.to_ipv6_mapped().into(), 0);
        let options = ScanOptions {
            timeout: Duration::from_secs(2),
            targets: vec![addr],
            bind,
            ..ScanOptions::default()
        };
        let Ok(socket) = super::bind(bind) else {
            eprintln!("skipped: no dual-stack IPv6 on this host");
            return;
        };
        drop(socket);
        let start = Instant::now();
        let devices = scan(&options).await.unwrap();
        // Ends at the answer, not at the timeout.
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(devices[0].addresses, [Address::from(addr.ip())]);
    }

    #[tokio::test]
    async fn ipv6_loopback_scan() {
        let Ok(responder) = UdpSocket::bind("[::1]:0").await else {
            eprintln!("skipped: no IPv6 on this host");
            return;
        };
        let addr = responder.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let (_, from) = responder.recv_from(&mut buf).await.unwrap();
                let mut r = Response::new();
                let inst = "Den._companion-link._tcp.local";
                r.ptr("_companion-link._tcp.local", inst)
                    .srv(inst, 49153, "den.local")
                    .txt(inst, &[])
                    .aaaa("den.local", "fe80::1".parse().unwrap());
                responder.send_to(&r.finish(), from).await.unwrap();
            }
        });
        let options = ScanOptions {
            timeout: Duration::from_millis(1500),
            targets: vec![addr],
            bind: (std::net::Ipv6Addr::LOCALHOST, 0).into(),
            ..ScanOptions::default()
        };
        let devices = scan(&options).await.unwrap();
        assert_eq!(devices.len(), 1);
        // Loopback has no scope id, so the link-local address stays without
        // one, and the source address comes first.
        let addrs: Vec<_> = devices[0]
            .addresses
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(addrs, ["::1", "fe80::1"]);
        assert_eq!(
            devices[0].socket_addr(ServiceKind::Companion),
            Some("[::1]:49153".parse().unwrap())
        );
    }

    #[test]
    fn default_options() {
        let o = ScanOptions::default();
        assert_eq!(o.targets[0].port(), 5353);
        assert_eq!(o.services.len(), 4);
        assert_eq!(o.bind, "0.0.0.0:0".parse().unwrap());
        assert_eq!(MDNS_IPV6, "ff02::fb".parse::<std::net::Ipv6Addr>().unwrap());
    }
}
