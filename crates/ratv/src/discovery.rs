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

use ratv_discovery::{Collector, query};
pub use ratv_discovery::{
    DEFAULT_SERVICES, Device, MDNS_IPV4, MDNS_PORT, Service, ServiceKind, Txt,
};
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
    /// every target has answered with complete records.
    pub targets: Vec<SocketAddr>,
    /// Local address to send from; selects the network interface.
    pub bind: IpAddr,
    /// Service types to look for.
    pub services: Vec<ServiceKind>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
            targets: vec![SocketAddr::new(IpAddr::V4(MDNS_IPV4), MDNS_PORT)],
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            services: DEFAULT_SERVICES.to_vec(),
        }
    }
}

/// Scans the network and returns the devices that answered.
pub async fn scan(options: &ScanOptions) -> Result<Vec<Device>, Error> {
    let socket = UdpSocket::bind(SocketAddr::new(options.bind, 0)).await?;
    let initial = query(&options.services);
    let start = Instant::now();
    let deadline = start + options.timeout;
    let mut collector = Collector::new();
    let mut buf = vec![0u8; 9000];
    let mut next_send = Instant::now();
    let unicast = options.targets.iter().all(|t| !t.ip().is_multicast());
    let mut answered = vec![false; options.targets.len()];

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_send {
            // Ask again for anything still missing, otherwise repeat the first query.
            let msg = collector
                .follow_up_query()
                .unwrap_or_else(|| initial.clone());
            for target in &options.targets {
                if let Err(err) = socket.send_to(&msg, target).await {
                    tracing::debug!(%err, %target, "mDNS send failed");
                }
            }
            next_send = now + RESEND_INTERVAL;
        }
        let wait = deadline.min(next_send);
        match tokio::time::timeout_at(wait, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if let Err(err) = collector.add(&buf[..n], Some(from.ip())) {
                    tracing::debug!(%err, %from, "invalid mDNS response");
                    continue;
                }
                tracing::trace!(%from, len = n, complete = collector.is_complete(), "mDNS response");
                if unicast {
                    for (done, target) in answered.iter_mut().zip(&options.targets) {
                        *done |= target.ip() == from.ip();
                    }
                    if !collector.is_complete() {
                        // Ask for missing records now instead of after the resend interval.
                        next_send = Instant::now();
                    } else if answered.iter().all(|&a| a) {
                        break;
                    }
                }
            }
            Ok(Err(err)) => tracing::debug!(%err, "mDNS receive failed"),
            Err(_) => {}
        }
    }
    let devices = collector.devices();
    tracing::debug!(
        devices = devices.len(),
        elapsed = ?start.elapsed(),
        "scan done"
    );
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use ratv_discovery::dns::Response;

    use super::*;

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
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            ..ScanOptions::default()
        };
        let start = Instant::now();
        let devices = scan(&options).await.unwrap();
        // Ends when the records are complete, not after the timeout.
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name(), "Den");
        assert_eq!(devices[0].addresses, [IpAddr::V4(Ipv4Addr::LOCALHOST)]);
        assert_eq!(
            devices[0].service(ServiceKind::Companion).unwrap().port,
            49153
        );
    }

    #[test]
    fn default_options() {
        let o = ScanOptions::default();
        assert_eq!(o.targets[0].port(), 5353);
        assert_eq!(o.services.len(), 4);
    }
}
