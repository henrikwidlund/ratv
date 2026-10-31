use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

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
    fn split(instance: &str) -> Option<(Self, &str)> {
        Self::ALL.into_iter().find_map(|kind| {
            let suffix = kind.service_name();
            let cut = instance.len().checked_sub(suffix.len() + 1)?;
            let (label, rest) = instance.split_at(cut);
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

/// A discovered device.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Device {
    /// Host name, for example `Living-Room.local`.
    pub host: String,
    /// Addresses; IPv4 first.
    pub addresses: Vec<IpAddr>,
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

    /// Address of the first service of `kind`, using the first device address.
    #[must_use]
    pub fn socket_addr(&self, kind: ServiceKind) -> Option<SocketAddr> {
        Some(SocketAddr::new(
            *self.addresses.first()?,
            self.service(kind)?.port,
        ))
    }
}

#[derive(Debug, Default)]
struct Instance {
    // Name as received; map keys are lower case.
    name: String,
    srv: Option<(u16, String)>,
    txt: Option<Txt>,
    source: Option<IpAddr>,
}

/// Collects records from mDNS responses and groups them into devices.
#[derive(Debug, Default)]
pub struct Collector {
    instances: HashMap<String, Instance>,
    addresses: HashMap<String, Vec<IpAddr>>,
}

impl Collector {
    /// Creates an empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a response. `source` is the address it came from; it is used when
    /// the response has no A/AAAA record for the host.
    pub fn add(&mut self, msg: &[u8], source: Option<IpAddr>) -> Result<(), dns::Error> {
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
                RData::A(ip) => self.address(&record.name, IpAddr::V4(ip)),
                RData::Aaaa(ip) => self.address(&record.name, IpAddr::V6(ip)),
                _ => {}
            }
        }
        Ok(())
    }

    fn instance(&mut self, name: &str, source: Option<IpAddr>) -> &mut Instance {
        let entry = self.instances.entry(name.to_ascii_lowercase()).or_default();
        if entry.name.is_empty() {
            name.clone_into(&mut entry.name);
        }
        entry.source = entry.source.or(source);
        entry
    }

    fn address(&mut self, host: &str, ip: IpAddr) {
        let list = self.addresses.entry(host.to_ascii_lowercase()).or_default();
        if !list.contains(&ip) {
            list.push(ip);
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

    /// Returns all devices with at least one complete service.
    #[must_use]
    pub fn devices(&self) -> Vec<Device> {
        let mut devices: Vec<Device> = Vec::new();
        let mut keys: Vec<&String> = self.instances.keys().collect();
        keys.sort();
        for key in keys {
            let inst = &self.instances[key];
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
                    if addresses.is_empty() {
                        addresses.extend(inst.source);
                    }
                    addresses.sort_by_key(IpAddr::is_ipv6);
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
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            ]
        );
        assert_eq!(d.services.len(), 3);
        assert_eq!(d.identifier(), Some("AA:BB:CC:DD:EE:FF"));
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
        c.add(&r.finish(), Some(src)).unwrap();
        let d = &c.devices()[0];
        assert_eq!(d.name(), "Living Room");
        assert_eq!(d.addresses, [src]);
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
        assert_eq!(d.model(), Some("Apple TV"));
        assert_eq!(d.addresses, Vec::<IpAddr>::new());
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
        assert!(!c.is_complete());

        let mut r = Response::new();
        r.a(HOST, Ipv4Addr::new(10, 0, 0, 2));
        c.add(&r.finish(), None).unwrap();
        assert!(c.follow_up_query().is_none());
        assert!(c.is_complete());
    }

    #[test]
    fn query_has_all_services() {
        let q = crate::query(crate::DEFAULT_SERVICES);
        assert_eq!(q[4..6], [0, 4]);
        assert_eq!(ServiceKind::split("bad"), None);
        assert_eq!(ServiceKind::split("x_airplay._tcp.local"), None);
    }
}
