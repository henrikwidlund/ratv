//! Session keys for the three AirPlay connections, derived from the
//! pair-verify shared secret with HKDF-SHA512.

use ratv_hap::SharedSecret;

use crate::FrameCipher;

/// Which end of the connection the keys are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The controller (us).
    Client,
    /// The Apple TV. Used by fake devices in tests.
    Device,
}

/// Cipher for the control connection.
#[must_use]
pub fn control(secret: &SharedSecret, side: Side) -> FrameCipher {
    cipher(
        secret,
        b"Control-Salt",
        b"Control-Write-Encryption-Key",
        b"Control-Read-Encryption-Key",
        side,
    )
}

/// Cipher for the event connection. The names are swapped compared to the
/// control connection: the client writes with the "Read" key.
#[must_use]
pub fn events(secret: &SharedSecret, side: Side) -> FrameCipher {
    cipher(
        secret,
        b"Events-Salt",
        b"Events-Read-Encryption-Key",
        b"Events-Write-Encryption-Key",
        side,
    )
}

/// Cipher for the data stream with `seed` from the SETUP request.
#[must_use]
pub fn data_stream(secret: &SharedSecret, seed: u64, side: Side) -> FrameCipher {
    // Salt is "DataStream-Salt" followed by the seed in decimal.
    let mut salt = [0u8; 15 + 20];
    salt[..15].copy_from_slice(b"DataStream-Salt");
    let mut n = 15;
    for d in seed.to_string().bytes() {
        salt[n] = d;
        n += 1;
    }
    cipher(
        secret,
        &salt[..n],
        b"DataStream-Output-Encryption-Key",
        b"DataStream-Input-Encryption-Key",
        side,
    )
}

fn cipher(
    secret: &SharedSecret,
    salt: &[u8],
    client_write: &[u8],
    client_read: &[u8],
    side: Side,
) -> FrameCipher {
    let w = secret.derive(salt, client_write);
    let r = secret.derive(salt, client_read);
    match side {
        Side::Client => FrameCipher::new(&w, &r),
        Side::Device => FrameCipher::new(&r, &w),
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use ratv_hap::kdf::hkdf_sha512;

    use super::*;

    fn check(mut client: FrameCipher, mut device: FrameCipher) {
        let mut wire = BytesMut::new();
        client.seal(b"ping", &mut wire);
        let mut out = BytesMut::new();
        device.open(&mut wire, &mut out).unwrap();
        assert_eq!(&out[..], b"ping");
        device.seal(b"pong", &mut wire);
        out.clear();
        client.open(&mut wire, &mut out).unwrap();
        assert_eq!(&out[..], b"pong");
    }

    #[test]
    fn sides_match() {
        let s = SharedSecret::from_bytes(&[9; 32]);
        check(control(&s, Side::Client), control(&s, Side::Device));
        check(events(&s, Side::Client), events(&s, Side::Device));
        check(
            data_stream(&s, 42, Side::Client),
            data_stream(&s, 42, Side::Device),
        );
    }

    #[test]
    fn data_stream_salt_uses_decimal_seed() {
        let s = SharedSecret::from_bytes(&[9; 32]);
        let w: [u8; 32] = hkdf_sha512(
            b"DataStream-Salt18446744073709551615",
            b"DataStream-Output-Encryption-Key",
            s.as_bytes(),
        );
        let r: [u8; 32] = hkdf_sha512(
            b"DataStream-Salt18446744073709551615",
            b"DataStream-Input-Encryption-Key",
            s.as_bytes(),
        );
        check(
            data_stream(&s, u64::MAX, Side::Client),
            FrameCipher::new(&r, &w),
        );
    }
}
