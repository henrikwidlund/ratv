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
    let mut b: [u8; 16] = bytes();
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
    fn mac_format() {
        assert_eq!(mac_string([0xAA, 1, 2, 3, 4, 0xFF]), "AA:01:02:03:04:FF");
        let _ = u32();
    }
}
