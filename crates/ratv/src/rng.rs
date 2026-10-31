use std::fmt::Write;

use getrandom::SysRng;
use rand_core::{Rng, UnwrapErr};

/// Operating system random number generator. Panics if the OS source fails.
pub(crate) const fn os() -> UnwrapErr<SysRng> {
    UnwrapErr(SysRng)
}

pub(crate) fn u32() -> u32 {
    os().next_u32()
}

#[cfg(feature = "mrp")]
pub(crate) fn u64() -> u64 {
    os().next_u64()
}

pub(crate) fn bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    os().fill_bytes(&mut buf);
    buf
}

/// Random UUID v4 in upper case, for example `0A1B...-...`.
pub(crate) fn uuid() -> String {
    uuid_string(bytes())
}

/// Formats bytes as a UUID v4 in upper case. Sets the version and variant bits.
pub(crate) fn uuid_string(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{byte:02X}");
    }
    s
}

/// Formats bytes as `AA:BB:CC:DD:EE:FF`.
pub(crate) fn mac_string(mac: [u8; 6]) -> String {
    let mut s = String::with_capacity(17);
    for (i, b) in mac.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        let _ = write!(s, "{b:02X}");
    }
    s
}

/// FNV-1a hash, stable across Rust versions.
fn fnv(seed: u64, data: &[u8]) -> u64 {
    data.iter().fold(seed, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Device id `AA:BB:CC:DD:EE:FF` that stays the same for one pairing.
/// Some tvOS versions close the connection if it changes between connects.
pub(crate) fn stable_device_id(client_id: &[u8]) -> String {
    let mut id = [0; 6];
    id.copy_from_slice(&fnv(FNV_OFFSET, client_id).to_le_bytes()[..6]);
    // Locally administered, unicast.
    id[0] = (id[0] | 0x02) & !0x01;
    mac_string(id)
}

/// UUID that stays the same for one pairing.
#[cfg(feature = "mrp")]
pub(crate) fn stable_uuid(client_id: &[u8]) -> String {
    let mut b = [0; 16];
    b[..8].copy_from_slice(&fnv(FNV_OFFSET ^ 1, client_id).to_le_bytes());
    b[8..].copy_from_slice(&fnv(FNV_OFFSET ^ 2, client_id).to_le_bytes());
    uuid_string(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_format() {
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_eq!(u.matches('-').count(), 4);
        assert_ne!(uuid(), u);
    }

    #[test]
    fn stable_ids() {
        let a = b"E5B59DDB-9930-4ACC-BC01-BDB185F96216";
        let b = b"1404D88A-8D8A-49C1-82A4-5112791D0018";
        assert_eq!(stable_device_id(a), stable_device_id(a));
        assert_ne!(stable_device_id(a), stable_device_id(b));
        // Locally administered, unicast.
        assert_eq!(stable_device_id(a), "36:FB:79:9E:DA:F7");
    }

    #[cfg(feature = "mrp")]
    #[test]
    fn stable_uuid_per_pairing() {
        let a = b"E5B59DDB-9930-4ACC-BC01-BDB185F96216";
        let b = b"1404D88A-8D8A-49C1-82A4-5112791D0018";
        assert_eq!(stable_uuid(a), stable_uuid(a));
        assert_ne!(stable_uuid(a), stable_uuid(b));
        assert_eq!(&stable_uuid(a)[14..15], "4");
    }

    #[test]
    fn mac_format() {
        assert_eq!(mac_string([0xAA, 1, 2, 3, 4, 0xFF]), "AA:01:02:03:04:FF");
        let _ = u32();
    }
}
