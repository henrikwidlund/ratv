//! Lists Apple TV devices on the network.
//!
//! ```text
//! cargo run --example scan
//! cargo run --example scan -- 192.168.1.20   # ask one host directly
//! cargo run --example scan -- fe80::1%4       # link-local IPv6 with interface index
//! ```

#[path = "common/host.rs"]
mod host;

use std::net::{Ipv6Addr, SocketAddr};

use ratv::discovery::{ScanOptions, ServiceKind, scan};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = ScanOptions::default();
    let hosts: Vec<SocketAddr> = std::env::args()
        .skip(1)
        .map(|h| {
            host::mdns_addr(&h).ok_or_else(|| format!("not an IP or IPv6%interface-index: {h}"))
        })
        .collect::<Result<_, _>>()?;
    // An IPv6 target needs an IPv6 socket.
    if hosts.iter().any(SocketAddr::is_ipv6) {
        options.bind = (Ipv6Addr::UNSPECIFIED, 0).into();
    }
    if !hosts.is_empty() {
        options.targets = hosts;
    }
    for d in scan(&options).await? {
        println!("{} ({})", d.name(), d.host);
        println!("  id:      {}", d.identifier().unwrap_or("-"));
        println!("  model:   {}", d.model().unwrap_or("-"));
        println!("  os:      {}", d.os_version().unwrap_or("-"));
        for a in &d.addresses {
            println!("  address: {a}");
        }
        for kind in ServiceKind::ALL {
            if let Some(addr) = d.socket_addr(kind) {
                println!("  {kind:?}: {addr}");
            }
        }
    }
    Ok(())
}
