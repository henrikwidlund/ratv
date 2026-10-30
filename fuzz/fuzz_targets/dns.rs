//! mDNS response parser and device collector.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ratv_discovery::{Collector, ServiceKind, dns};

fuzz_target!(|data: &[u8]| {
    let _ = dns::parse(data);
    let mut c = Collector::new();
    if c.add(data, None).is_ok() {
        let _ = (c.follow_up_query(), c.is_complete());
        for device in c.devices() {
            let _ = (device.name(), device.model(), device.identifier());
            let _ = device.socket_addr(ServiceKind::Companion);
        }
    }
});
