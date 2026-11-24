use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};

use crate::dns::{self, RData, RecordType};
use crate::txt::{Txt, parse_features};

/// Service types used by Apple TV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ServiceKind {
    /// Companion protocol (`_companion-link._tcp`).
    Companion,
    /// AirPlay 2 (`_airplay._tcp`). MRP runs inside it on tvOS 15+.
    AirPlay,
    /// MRP over plain TCP (`_mediaremotetv._tcp`), tvOS 14 and older.
    MediaRemote,
    /// AirPlay 1 audio (`_raop._tcp`).
    Raop,
}

impl ServiceKind {
    /// All known kinds.
    pub const ALL: [Self; 4] = [
        Self::Companion,
        Self::AirPlay,
        Self::MediaRemote,
        Self::Raop,
    ];

    /// mDNS service name, for example `_airplay._tcp.local`.
    #[must_use]
    pub const fn service_name(self) -> &'static str {
        match self {
            Self::Companion => "_companion-link._tcp.local",
            Self::AirPlay => "_airplay._tcp.local",
            Self::MediaRemote => "_mediaremotetv._tcp.local",
            Self::Raop => "_raop._tcp.local",
        }
    }

    /// Splits an instance name into the kind and the instance label.
    /// None if the label is empty.
    fn split(instance: &str) -> Option<(Self, &str)> {
        Self::ALL.into_iter().find_map(|kind| {
            let suffix = kind.service_name();
            let cut = instance
                .len()
                .checked_sub(suffix.len() + 1)
                .filter(|&cut| cut > 0)?;
            // None when `cut` is inside a multi-byte char: the name cannot match.
            let (label, rest) = instance.split_at_checked(cut)?;
            (rest.as_bytes()[0] == b'.' && rest[1..].eq_ignore_ascii_case(suffix))
                .then_some((kind, label))
        })
    }
}

/// A service on a device.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Service {
    /// Kind.
    pub kind: ServiceKind,
    /// Instance label, for example `Living Room`.
    pub name: String,
    /// TCP port.
    pub port: u16,
    /// TXT properties.
    pub txt: Txt,
}

/// An IP address of a device and its IPv6 scope id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address {
    /// IP address. The collector stores an IPv4-mapped IPv6 address
    /// (`::ffff:a.b.c.d`) as IPv4.
    pub ip: IpAddr,
    /// IPv6 scope id (interface index) of the link where the address was
    /// received, or 0 if not known. Only link-local IPv6 addresses
    /// (`fe80::/10`) have one; the collector sets it to 0 for all other
    /// addresses.
    pub scope_id: u32,
}

impl Address {
    /// Sets `scope_id` only for a link-local IPv6 address.
    const fn scoped(ip: IpAddr, scope_id: u32) -> Self {
        let scope_id = match ip {
            IpAddr::V6(v6) if v6.is_unicast_link_local() => scope_id,
            _ => 0,
        };
        Self { ip, scope_id }
    }

    /// Socket address with `port`. An IPv6 address keeps the scope id.
    #[must_use]
    pub const fn socket_addr(self, port: u16) -> SocketAddr {
        match self.ip {
            IpAddr::V6(ip) => SocketAddr::V6(SocketAddrV6::new(ip, port, 0, self.scope_id)),
            ip @ IpAddr::V4(_) => SocketAddr::new(ip, port),
        }
    }
}

impl From<IpAddr> for Address {
    fn from(ip: IpAddr) -> Self {
        Self { ip, scope_id: 0 }
    }
}

/// Shows the IP, with `%scope_id` when the scope id is not 0.
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.scope_id {
            0 => self.ip.fmt(f),
            scope => write!(f, "{}%{scope}", self.ip),
        }
    }
}

/// A discovered device.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Device {
    /// Host name, ASCII letters in lower case, for example `living-room.local`.
    pub host: String,
    /// Addresses; IPv4 first, then other IPv6, then link-local IPv6 with a
    /// scope id, then link-local IPv6 without one. A link-local IPv6 address
    /// from AAAA records of two links is listed once for each link. Can hold
    /// the address the answer came from, see [`Collector::add`]. IPv4-mapped
    /// IPv6 addresses are stored as IPv4. Unspecified, multicast and IPv4
    /// broadcast addresses are left out.
    pub addresses: Vec<Address>,
    /// Services.
    pub services: Vec<Service>,
}

impl Device {
    /// Returns the service of `kind`.
    #[must_use]
    pub fn service(&self, kind: ServiceKind) -> Option<&Service> {
        self.services.iter().find(|s| s.kind == kind)
    }

    /// Display name from the first service found.
    #[must_use]
    pub fn name(&self) -> &str {
        [
            ServiceKind::Companion,
            ServiceKind::AirPlay,
            ServiceKind::MediaRemote,
        ]
        .into_iter()
        .find_map(|k| self.service(k))
        .map(|s| s.name.as_str())
        .or_else(|| {
            self.service(ServiceKind::Raop)
                .map(|s| s.name.split_once('@').map_or(s.name.as_str(), |(_, n)| n))
        })
        .unwrap_or(&self.host)
    }

    fn txt(&self, kind: ServiceKind, key: &str) -> Option<&str> {
        self.service(kind)?.txt.get(key)
    }

    /// Stable identifier: AirPlay `deviceid`, else MRP or Companion ids.
    #[must_use]
    pub fn identifier(&self) -> Option<&str> {
        self.txt(ServiceKind::AirPlay, "deviceid")
            .or_else(|| self.txt(ServiceKind::MediaRemote, "UniqueIdentifier"))
            .or_else(|| self.txt(ServiceKind::Companion, "rpMRtID"))
    }

    /// UID of this device in MRP output device lists and messages, from AirPlay `psi`.
    /// Use it with `add_outputs`, `remove_outputs` and `set_outputs`.
    #[must_use]
    pub fn output_uid(&self) -> Option<&str> {
        self.txt(ServiceKind::AirPlay, "psi")
    }

    /// Model identifier, for example `AppleTV14,1`.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.txt(ServiceKind::AirPlay, "model")
            .or_else(|| self.txt(ServiceKind::Companion, "rpMd"))
            .or_else(|| self.txt(ServiceKind::MediaRemote, "ModelName"))
    }

    /// OS version from AirPlay.
    #[must_use]
    pub fn os_version(&self) -> Option<&str> {
        self.txt(ServiceKind::AirPlay, "osvers")
    }

    /// AirPlay feature bits.
    #[must_use]
    pub fn airplay_features(&self) -> Option<u64> {
        self.txt(ServiceKind::AirPlay, "features")
            .and_then(parse_features)
    }

    /// Address of the first service of `kind`, using the first device address
    /// and its [`scope_id`](Address::scope_id). This can be a link-local IPv6
    /// address with scope id 0, which cannot be used to connect, when no other
    /// address is known.
    #[must_use]
    pub fn socket_addr(&self, kind: ServiceKind) -> Option<SocketAddr> {
        let port = self.service(kind)?.port;
        Some(self.addresses.first()?.socket_addr(port))
    }
}

/// False for addresses that cannot reach a device: unspecified, multicast
/// and the IPv4 broadcast address.
const fn usable(ip: IpAddr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && !matches!(ip, IpAddr::V4(v4) if v4.is_broadcast())
}

/// Sort key: IPv4, then IPv6 that is not link-local, then link-local with a
/// scope id, then link-local without one, which cannot be used to connect.
/// The fuzz target `dns` has a copy of this.
const fn address_rank(a: Address) -> u8 {
    match a.ip {
        IpAddr::V4(_) => 0,
        IpAddr::V6(ip) if !ip.is_unicast_link_local() => 1,
        IpAddr::V6(_) if a.scope_id != 0 => 2,
        IpAddr::V6(_) => 3,
    }
}

#[derive(Debug, Default)]
struct Instance {
    // Name as received; map keys are lower case.
    name: String,
    srv: Option<(u16, String)>,
    txt: Option<Txt>,
    source: Option<Address>,
}

/// Collects records from mDNS responses and groups them into devices.
#[derive(Debug, Default)]
pub struct Collector {
    instances: HashMap<String, Instance>,
    addresses: HashMap<String, Vec<Address>>,
}

impl Collector {
    /// Creates an empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a response. `source` is the address it came from; pass it on each
    /// call. The first source of a service is kept. Its address (with scope
    /// id) is used when the host has no address to connect to: no kept A/AAAA
    /// record, or only link-local IPv6 without a scope id. The scope id of an
    /// IPv6 `source` is given to the link-local addresses of this response (see
    /// [`Address::scope_id`]). The port is not used. A device uses only the
    /// source of its first service, in order of the instance name, ASCII
    /// case ignored. An unspecified, multicast or IPv4 broadcast `source` is
    /// not used as an address; its scope id is still used.
    pub fn add(&mut self, msg: &[u8], source: Option<SocketAddr>) -> Result<(), dns::Error> {
        let scope_id = match source {
            Some(SocketAddr::V6(a)) => a.scope_id(),
            _ => 0,
        };
        let source = source
            .map(|a| Address::scoped(a.ip().to_canonical(), scope_id))
            .filter(|a| usable(a.ip));
        for record in dns::parse(msg)? {
            match record.data {
                RData::Ptr(instance) if record.ty == RecordType::PTR => {
                    if ServiceKind::split(&instance).is_some() {
                        self.instance(&instance, source);
                    }
                }
                // Other services, such as `_device-info._tcp`, would never be complete.
                RData::Srv { .. } | RData::Txt(_) if ServiceKind::split(&record.name).is_none() => {
                }
                RData::Srv { port, target } => {
                    let entry = self.instance(&record.name, source);
                    entry.srv = Some((port, target.to_ascii_lowercase()));
                }
                RData::Txt(data) => {
                    self.instance(&record.name, source).txt = Some(Txt::parse(data));
                }
                RData::A(ip) => self.address(&record.name, IpAddr::V4(ip).into()),
                // An IPv4-mapped address counts as IPv4.
                RData::Aaaa(ip) => {
                    let ip = IpAddr::V6(ip).to_canonical();
                    self.address(&record.name, Address::scoped(ip, scope_id));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn instance(&mut self, name: &str, source: Option<Address>) -> &mut Instance {
        let entry = self.instances.entry(name.to_ascii_lowercase()).or_default();
        if entry.name.is_empty() {
            name.clone_into(&mut entry.name);
        }
        entry.source = entry.source.or(source);
        entry
    }

    /// Adds `addr` to `host`. A known scope id replaces 0 for the same IP,
    /// and an address with scope id 0 is not added when the IP is known.
    /// Addresses that are not [`usable`] are left out.
    fn address(&mut self, host: &str, addr: Address) {
        if !usable(addr.ip) {
            return;
        }
        let list = self.addresses.entry(host.to_ascii_lowercase()).or_default();
        let mut same_ip = list.iter_mut().filter(|a| a.ip == addr.ip);
        if addr.scope_id == 0 {
            if same_ip.next().is_none() {
                list.push(addr);
            }
        } else if let Some(a) = same_ip.find(|a| a.scope_id == 0 || a.scope_id == addr.scope_id) {
            a.scope_id = addr.scope_id;
        } else {
            list.push(addr);
        }
    }

    /// Builds a query for SRV/TXT/A records that are still missing, if any.
    #[must_use]
    pub fn follow_up_query(&self) -> Option<Vec<u8>> {
        let mut q = dns::Query::new();
        let mut any = false;
        for (name, ty) in self.missing() {
            q.question(name, ty);
            any = true;
        }
        any.then(|| q.finish())
    }

    /// True when at least one service was found and no records are missing.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.instances.is_empty() && self.missing().next().is_none()
    }

    /// Names and types of records that are still missing.
    fn missing(&self) -> impl Iterator<Item = (&str, RecordType)> {
        self.instances.iter().flat_map(|(name, inst)| {
            let host = inst
                .srv
                .as_ref()
                .filter(|(_, host)| !self.addresses.contains_key(host) && inst.source.is_none())
                .map(|(_, host)| (host.as_str(), RecordType::A));
            [
                inst.srv
                    .is_none()
                    .then_some((name.as_str(), RecordType::SRV)),
                inst.txt
                    .is_none()
                    .then_some((name.as_str(), RecordType::TXT)),
                host,
            ]
            .into_iter()
            .flatten()
        })
    }

    /// Returns all devices with at least one service that has a SRV record.
    #[must_use]
    pub fn devices(&self) -> Vec<Device> {
        self.build(|_| true)
    }

    /// Like [`devices`](Self::devices), but uses only services with all
    /// records: SRV, TXT and an address.
    #[must_use]
    pub fn complete_devices(&self) -> Vec<Device> {
        self.build(|inst| {
            inst.txt.is_some()
                && inst.srv.as_ref().is_some_and(|(_, host)| {
                    inst.source.is_some() || self.addresses.contains_key(host)
                })
        })
    }

    fn build(&self, include: impl Fn(&Instance) -> bool) -> Vec<Device> {
        let mut devices: Vec<Device> = Vec::new();
        let mut keys: Vec<&String> = self.instances.keys().collect();
        keys.sort();
        for key in keys {
            let inst = &self.instances[key];
            if !include(inst) {
                continue;
            }
            let (Some((port, host)), Some((kind, label))) =
                (&inst.srv, ServiceKind::split(&inst.name))
            else {
                continue;
            };
            let service = Service {
                kind,
                name: label.to_owned(),
                port: *port,
                txt: inst.txt.clone().unwrap_or_default(),
            };
            let index = devices
                .iter()
                .position(|d| d.host == *host)
                .unwrap_or_else(|| {
                    let mut addresses = self.addresses.get(host).cloned().unwrap_or_default();
                    // The source helps when no address can be used: none, or
                    // only link-local IPv6 without a scope id.
                    if addresses.iter().all(|a| address_rank(*a) == 3)
                        && let Some(src) = inst.source
                    {
                        addresses.retain(|a| a.ip != src.ip);
                        addresses.push(src);
                    }
                    addresses.sort_by_key(|a| address_rank(*a));
                    devices.push(Device {
                        host: host.clone(),
                        addresses,
                        services: Vec::new(),
                    });
                    devices.len() - 1
                });
            devices[index].services.push(service);
        }
        devices
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;
    use crate::MDNS_PORT;
    use crate::dns::Response;

    const CL: &str = "Living Room._companion-link._tcp.local";
    const AP: &str = "Living Room._airplay._tcp.local";
    const RAOP: &str = "AABBCCDDEEFF@Living Room._raop._tcp.local";
    const HOST: &str = "Living-Room.local";

    fn full_response() -> Vec<u8> {
        let mut r = Response::new();
        r.ptr("_companion-link._tcp.local", CL)
            .srv(CL, 49153, HOST)
            .txt(CL, &["rpMd=AppleTV14,1", "rpMRtID=ABC"])
            .ptr("_airplay._tcp.local", AP)
            .srv(AP, 7000, HOST)
            .txt(
                AP,
                &[
                    "deviceid=AA:BB:CC:DD:EE:FF",
                    "features=0x4A7FDFD5,0xBC157FDE",
                    "model=AppleTV14,1",
                    "osvers=26.0",
                    "psi=DC737A0E-DD5A-4106-8EF9-26D98ED1AEC0",
                ],
            )
            .ptr("_raop._tcp.local", RAOP)
            .srv(RAOP, 7000, HOST)
            .txt(RAOP, &["am=AppleTV14,1"])
            .aaaa(HOST, Ipv6Addr::LOCALHOST)
            .a(HOST, Ipv4Addr::new(10, 0, 0, 2))
            .a(HOST, Ipv4Addr::new(10, 0, 0, 2));
        r.finish()
    }

    #[test]
    fn groups_services_by_host() {
        let mut c = Collector::new();
        c.add(&full_response(), None).unwrap();
        assert!(c.follow_up_query().is_none());
        let devices = c.devices();
        assert_eq!(devices.len(), 1);
        let d = &devices[0];
        assert_eq!(d.name(), "Living Room");
        assert_eq!(d.host, "living-room.local");
        assert_eq!(
            d.addresses,
            [
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)).into(),
                IpAddr::V6(Ipv6Addr::LOCALHOST).into()
            ]
        );
        assert_eq!(d.services.len(), 3);
        assert_eq!(d.identifier(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(d.output_uid(), Some("DC737A0E-DD5A-4106-8EF9-26D98ED1AEC0"));
        assert_eq!(d.model(), Some("AppleTV14,1"));
        assert_eq!(d.os_version(), Some("26.0"));
        assert_eq!(d.airplay_features(), Some(0xBC15_7FDE_4A7F_DFD5));
        assert_eq!(
            d.socket_addr(ServiceKind::Companion),
            Some("10.0.0.2:49153".parse().unwrap())
        );
        assert_eq!(d.socket_addr(ServiceKind::MediaRemote), None);
    }

    #[test]
    fn fallbacks() {
        let mut r = Response::new();
        r.srv(RAOP, 7000, HOST);
        let mut c = Collector::new();
        let src = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9));
        c.add(&r.finish(), Some(SocketAddr::new(src, MDNS_PORT)))
            .unwrap();
        let d = &c.devices()[0];
        assert_eq!(d.name(), "Living Room");
        assert_eq!(d.addresses, [src.into()]);
        assert_eq!(d.identifier(), None);
        assert_eq!(d.model(), None);

        let mut r = Response::new();
        r.srv("Den._mediaremotetv._tcp.local", 49152, "den.local")
            .txt(
                "Den._mediaremotetv._tcp.local",
                &["UniqueIdentifier=U1", "ModelName=Apple TV"],
            );
        let mut c = Collector::new();
        c.add(&r.finish(), None).unwrap();
        let d = &c.devices()[0];
        assert_eq!(d.identifier(), Some("U1"));
        assert_eq!(d.output_uid(), None);
        assert_eq!(d.model(), Some("Apple TV"));
        assert_eq!(d.addresses, []);
        assert_eq!(d.socket_addr(ServiceKind::MediaRemote), None);

        let d = Device {
            host: "h".into(),
            addresses: vec![],
            services: vec![],
        };
        assert_eq!(d.name(), "h");
    }

    #[test]
    fn follow_up_for_missing_records() {
        let mut r = Response::new();
        r.ptr("_airplay._tcp.local", AP)
            .ptr("_other._tcp.local", "x._other._tcp.local");
        let mut c = Collector::new();
        assert!(!c.is_complete());
        // Unknown services are ignored.
        let mut info = Response::new();
        info.txt("Living Room._device-info._tcp.local", &["model=J105aAP"]);
        c.add(&info.finish(), None).unwrap();
        assert!(c.follow_up_query().is_none());
        c.add(&r.finish(), None).unwrap();
        assert_eq!(c.devices(), []);
        assert!(!c.is_complete());
        let q = c.follow_up_query().unwrap();
        assert_eq!(q[4..6], [0, 2]);

        let mut r = Response::new();
        r.srv(AP, 7000, HOST).txt(AP, &[]);
        c.add(&r.finish(), None).unwrap();
        let q = c.follow_up_query().unwrap();
        assert_eq!(q[4..6], [0, 1]);
        assert_eq!(c.devices()[0].addresses.len(), 0);
        assert_eq!(c.complete_devices(), []);
        assert!(!c.is_complete());

        let mut r = Response::new();
        r.a(HOST, Ipv4Addr::new(10, 0, 0, 2));
        c.add(&r.finish(), None).unwrap();
        assert!(c.follow_up_query().is_none());
        assert!(c.is_complete());
        assert_eq!(c.complete_devices(), c.devices());
    }

    #[test]
    fn query_has_all_services() {
        let q = crate::query(crate::DEFAULT_SERVICES);
        assert_eq!(q[4..6], [0, 4]);
        assert_eq!(ServiceKind::split("bad"), None);
        assert_eq!(ServiceKind::split("x_airplay._tcp.local"), None);
    }

    #[test]
    fn empty_label_is_no_service() {
        for kind in ServiceKind::ALL {
            let inst = format!(".{}", kind.service_name());
            assert_eq!(ServiceKind::split(&inst), None);
            assert_eq!(ServiceKind::split(&inst[1..]), None);
        }
    }

    #[test]
    fn mapped_source_is_ipv4() {
        let mut r = Response::new();
        r.srv(CL, 49153, HOST).txt(CL, &[]);
        let mut c = Collector::new();
        c.add(&r.finish(), Some("[::ffff:10.0.0.9]:5353".parse().unwrap()))
            .unwrap();
        let d = &c.devices()[0];
        assert_eq!(d.addresses, [Address::from(IpAddr::from([10, 0, 0, 9]))]);
    }

    #[test]
    fn unusable_addresses_left_out() {
        let mut r = Response::new();
        r.srv(CL, 49153, HOST)
            .txt(CL, &[])
            .a(HOST, Ipv4Addr::UNSPECIFIED)
            .a(HOST, Ipv4Addr::new(224, 0, 0, 251))
            .a(HOST, Ipv4Addr::BROADCAST)
            .aaaa(HOST, Ipv6Addr::UNSPECIFIED)
            .aaaa(HOST, "ff02::fb".parse().unwrap())
            .aaaa(HOST, Ipv4Addr::UNSPECIFIED.to_ipv6_mapped());
        let msg = r.finish();
        let mut c = Collector::new();
        c.add(&msg, Some("0.0.0.0:5353".parse().unwrap())).unwrap();
        // No address is known yet, so A is still asked for (one question).
        assert!(!c.is_complete());
        assert_eq!(c.follow_up_query().unwrap()[4..6], [0, 1]);
        assert_eq!(c.complete_devices(), []);
        assert_eq!(c.devices()[0].addresses, []);

        // A usable source is used instead.
        c.add(&msg, Some("10.0.0.9:5353".parse().unwrap())).unwrap();
        assert!(c.is_complete());
        let d = &c.complete_devices()[0];
        assert_eq!(d.addresses, [Address::from(IpAddr::from([10, 0, 0, 9]))]);
    }

    #[test]
    fn mapped_aaaa_is_ipv4() {
        let mut r = Response::new();
        r.srv(CL, 49153, HOST)
            .txt(CL, &[])
            .a(HOST, Ipv4Addr::new(10, 0, 0, 1))
            .aaaa(HOST, Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped());
        let mut c = Collector::new();
        c.add(&r.finish(), None).unwrap();
        let d = &c.devices()[0];
        assert_eq!(d.addresses, [Address::from(IpAddr::from([10, 0, 0, 1]))]);

        // A mapped AAAA alone, from a scoped IPv6 source: IPv4 with scope id
        // 0, and the source is not added.
        let mut r = Response::new();
        r.srv(CL, 49153, HOST)
            .txt(CL, &[])
            .aaaa(HOST, Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped());
        let mut c = Collector::new();
        let source = SocketAddr::V6(SocketAddrV6::new("fe80::9".parse().unwrap(), 5353, 0, 3));
        c.add(&r.finish(), Some(source)).unwrap();
        let d = &c.devices()[0];
        assert_eq!(d.addresses, [Address::from(IpAddr::from([10, 0, 0, 1]))]);
    }

    #[test]
    fn scoped_link_local_before_unscoped() {
        let ll_a: Ipv6Addr = "fe80::a".parse().unwrap();
        let ll_b: Ipv6Addr = "fe80::b".parse().unwrap();
        let mut c = Collector::new();
        // From an IPv4 packet: no scope id.
        let mut r = Response::new();
        r.srv(CL, 49153, HOST).txt(CL, &[]).aaaa(HOST, ll_a);
        c.add(&r.finish(), Some("10.0.0.2:5353".parse().unwrap()))
            .unwrap();
        let mut r = Response::new();
        r.aaaa(HOST, ll_b);
        let src = SocketAddr::V6(SocketAddrV6::new(ll_b, MDNS_PORT, 0, 5));
        c.add(&r.finish(), Some(src)).unwrap();
        let d = &c.devices()[0];
        assert_eq!(d.addresses[0].to_string(), "fe80::b%5");
        assert_eq!(d.addresses[1].to_string(), "fe80::a");
    }

    #[test]
    fn source_when_only_unscoped_link_local() {
        let ll: Ipv6Addr = "fe80::a".parse().unwrap();
        let mut r = Response::new();
        r.srv(CL, 49153, HOST).txt(CL, &[]).aaaa(HOST, ll);
        let mut c = Collector::new();
        // From an IPv4 packet: the AAAA record has no scope id.
        c.add(&r.finish(), Some("10.0.0.2:5353".parse().unwrap()))
            .unwrap();
        let d = &c.devices()[0];
        assert_eq!(d.addresses[0].to_string(), "10.0.0.2");
        assert_eq!(d.addresses[1].to_string(), "fe80::a");
        assert_eq!(c.complete_devices()[0].addresses, d.addresses);

        // The first source of a service is kept.
        let mut c = Collector::new();
        let mut r = Response::new();
        r.srv(CL, 49153, HOST).aaaa(HOST, ll);
        c.add(&r.finish(), Some("10.0.0.2:5353".parse().unwrap()))
            .unwrap();
        let mut r = Response::new();
        r.txt(CL, &[]);
        c.add(&r.finish(), Some("10.0.0.3:5353".parse().unwrap()))
            .unwrap();
        assert_eq!(c.devices()[0].addresses[0].to_string(), "10.0.0.2");

        // The same IP with a scope id replaces the unscoped one.
        let mut c = Collector::new();
        let mut r = Response::new();
        r.aaaa(HOST, ll);
        c.add(&r.finish(), Some("10.0.0.2:5353".parse().unwrap()))
            .unwrap();
        let mut r = Response::new();
        r.srv(CL, 49153, HOST).txt(CL, &[]);
        let src = SocketAddr::V6(SocketAddrV6::new(ll, MDNS_PORT, 0, 5));
        c.add(&r.finish(), Some(src)).unwrap();
        assert_eq!(c.devices()[0].addresses.len(), 1);
        assert_eq!(c.devices()[0].addresses[0].to_string(), "fe80::a%5");
    }

    #[test]
    fn split_non_ascii() {
        assert_eq!(
            ServiceKind::split("Wohnzimmer Ü._airplay._tcp.local"),
            Some((ServiceKind::AirPlay, "Wohnzimmer Ü"))
        );
        assert_eq!(
            ServiceKind::split("😀._raop._tcp.local"),
            Some((ServiceKind::Raop, "😀"))
        );
        // Multi-byte chars at the cut, for each kind.
        for kind in ServiceKind::ALL {
            let len = kind.service_name().len() + 1;
            for name in ["é", "€", "😀", "éé", "a€", "€a"] {
                let s = name.repeat(len);
                assert_eq!(ServiceKind::split(&s), None);
                let s = format!("{name}{}", &kind.service_name()[1..]);
                assert_eq!(ServiceKind::split(&s), None);
            }
        }
        let mut r = Response::new();
        let inst = "Kök 😀._companion-link._tcp.local";
        r.ptr("_companion-link._tcp.local", inst)
            .srv(inst, 49153, HOST)
            .txt(inst, &[]);
        let mut c = Collector::new();
        c.add(&r.finish(), None).unwrap();
        assert_eq!(c.devices()[0].name(), "Kök 😀");
    }

    proptest::proptest! {
        #[test]
        fn split_never_panics(label in "\\PC*", tail in "\\PC{0,30}") {
            let _ = ServiceKind::split(&label);
            let _ = ServiceKind::split(&format!("{label}{tail}._tcp.local"));
        }
    }

    #[test]
    fn scope_id_for_link_local() {
        let ll: Ipv6Addr = "fe80::1".parse().unwrap();
        let src = SocketAddr::V6(SocketAddrV6::new(ll, MDNS_PORT, 0, 7));
        // Address from the source.
        let mut r = Response::new();
        r.srv(CL, 49153, HOST).txt(CL, &[]);
        let mut c = Collector::new();
        c.add(&r.finish(), Some(src)).unwrap();
        let d = &c.devices()[0];
        assert_eq!(
            d.addresses,
            [Address {
                ip: ll.into(),
                scope_id: 7
            }]
        );
        assert_eq!(d.addresses[0].to_string(), "fe80::1%7");
        assert_eq!(
            d.socket_addr(ServiceKind::Companion),
            Some("[fe80::1%7]:49153".parse().unwrap())
        );

        // Address from an AAAA record.
        let mut r = Response::new();
        r.srv(CL, 49153, HOST)
            .txt(CL, &[])
            .aaaa(HOST, "fe80::2".parse().unwrap());
        let mut c = Collector::new();
        c.add(&r.finish(), Some(src)).unwrap();
        assert_eq!(
            c.devices()[0].socket_addr(ServiceKind::Companion),
            Some("[fe80::2%7]:49153".parse().unwrap())
        );

        // Not link-local: no scope.
        let mut r = Response::new();
        r.srv(CL, 49153, HOST)
            .txt(CL, &[])
            .aaaa(HOST, "2001:db8::1".parse().unwrap());
        let mut c = Collector::new();
        c.add(&r.finish(), Some(src)).unwrap();
        let addr = c.devices()[0].socket_addr(ServiceKind::Companion).unwrap();
        assert_eq!(addr, "[2001:db8::1]:49153".parse().unwrap());
        let SocketAddr::V6(v6) = addr else { panic!() };
        assert_eq!(v6.scope_id(), 0);

        // Interface not known: no scope id.
        let mut r = Response::new();
        r.srv(CL, 49153, HOST).txt(CL, &[]).aaaa(HOST, ll);
        let mut c = Collector::new();
        c.add(&r.finish(), None).unwrap();
        assert_eq!(c.devices()[0].addresses[0].scope_id, 0);
        assert_eq!(c.devices()[0].addresses[0].to_string(), "fe80::1");
        assert_eq!(
            c.devices()[0].socket_addr(ServiceKind::Companion),
            Some("[fe80::1]:49153".parse().unwrap())
        );

        let msg = || {
            let mut r = Response::new();
            r.srv(CL, 49153, HOST).txt(CL, &[]).aaaa(HOST, ll);
            r.finish()
        };

        // IPv4 source: no scope.
        let v4 = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 9), MDNS_PORT));
        let mut c = Collector::new();
        c.add(&msg(), Some(v4)).unwrap();
        assert_eq!(c.devices()[0].addresses[0].scope_id, 0);

        // A known scope replaces 0, and 0 does not add the IP again.
        let mut c = Collector::new();
        c.add(&msg(), None).unwrap();
        c.add(&msg(), Some(src)).unwrap();
        c.add(&msg(), None).unwrap();
        assert_eq!(
            c.devices()[0].addresses,
            [Address {
                ip: ll.into(),
                scope_id: 7
            }]
        );

        // The same IP on two links: one address per link.
        let other = SocketAddr::V6(SocketAddrV6::new(ll, MDNS_PORT, 0, 9));
        let mut c = Collector::new();
        c.add(&msg(), Some(src)).unwrap();
        c.add(&msg(), Some(other)).unwrap();
        c.add(&msg(), Some(src)).unwrap();
        assert_eq!(
            c.devices()[0].addresses,
            [
                Address {
                    ip: ll.into(),
                    scope_id: 7
                },
                Address {
                    ip: ll.into(),
                    scope_id: 9
                }
            ]
        );
    }

    #[test]
    fn scope_per_address() {
        let ll_a: Ipv6Addr = "fe80::a".parse().unwrap();
        let ll_b: Ipv6Addr = "fe80::b".parse().unwrap();
        let v4 = Ipv4Addr::new(10, 0, 0, 2);
        let global: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let mut c = Collector::new();
        // Each link-local address gets the scope of the packet it came in.
        let mut r = Response::new();
        r.srv(CL, 49153, HOST)
            .txt(CL, &[])
            .aaaa(HOST, ll_a)
            .a(HOST, v4);
        let src = SocketAddr::V6(SocketAddrV6::new(ll_a, MDNS_PORT, 0, 4));
        c.add(&r.finish(), Some(src)).unwrap();
        let mut r = Response::new();
        r.aaaa(HOST, ll_b).aaaa(HOST, global).a(HOST, v4);
        let src = SocketAddr::V6(SocketAddrV6::new(ll_b, MDNS_PORT, 0, 5));
        c.add(&r.finish(), Some(src)).unwrap();
        let d = &c.devices()[0];
        assert_eq!(
            d.addresses,
            [
                Address {
                    ip: v4.into(),
                    scope_id: 0
                },
                Address {
                    ip: global.into(),
                    scope_id: 0
                },
                Address {
                    ip: ll_a.into(),
                    scope_id: 4
                },
                Address {
                    ip: ll_b.into(),
                    scope_id: 5
                },
            ]
        );
        assert_eq!(
            d.socket_addr(ServiceKind::Companion),
            Some("10.0.0.2:49153".parse().unwrap())
        );
        assert_eq!(
            d.addresses[3].socket_addr(1),
            "[fe80::b%5]:1".parse().unwrap()
        );
        assert_eq!(d.addresses[0].to_string(), "10.0.0.2");

        // An IPv4 address from the source has no scope.
        let mut c = Collector::new();
        let mut r = Response::new();
        r.srv(CL, 49153, HOST).txt(CL, &[]);
        let src = SocketAddr::new(v4.into(), MDNS_PORT);
        c.add(&r.finish(), Some(src)).unwrap();
        assert_eq!(
            c.devices()[0].addresses,
            [Address {
                ip: v4.into(),
                scope_id: 0
            }]
        );
    }
}
