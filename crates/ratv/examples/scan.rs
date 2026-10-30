//! Lists Apple TV devices on the network.
//!
//! ```text
//! cargo run --example scan
//! cargo run --example scan -- 192.168.1.20   # ask one host directly
//! ```

use std::net::{IpAddr, SocketAddr};

use ratv::discovery::{MDNS_PORT, ScanOptions, ServiceKind, scan};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = ScanOptions::default();
    let hosts: Vec<SocketAddr> = std::env::args()
        .skip(1)
        .map(|h| h.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, MDNS_PORT)))
        .collect::<Result<_, _>>()?;
    if !hosts.is_empty() {
        options.targets = hosts;
    }
    for d in scan(&options).await? {
        println!("{} ({})", d.name(), d.host);
        println!("  id:      {}", d.identifier().unwrap_or("-"));
        println!("  model:   {}", d.model().unwrap_or("-"));
        println!("  os:      {}", d.os_version().unwrap_or("-"));
        println!("  address: {:?}", d.addresses);
        for kind in ServiceKind::ALL {
            if let Some(addr) = d.socket_addr(kind) {
                println!("  {kind:?}: {addr}");
            }
        }
    }
    Ok(())
}
