//! HKDF-SHA512 key derivation and the salt/info labels used by HAP.

use hkdf::Hkdf;
use sha2::Sha512;

/// Derives `N` bytes with HKDF-SHA512.
///
/// # Panics
/// Panics if `N` is larger than 255 * 64 bytes.
#[must_use]
pub fn hkdf_sha512<const N: usize>(salt: &[u8], info: &[u8], ikm: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    Hkdf::<Sha512>::new(Some(salt), ikm)
        .expand(info, &mut out)
        .expect("valid HKDF output length");
    out
}

/// Salt and info labels.
#[allow(missing_docs)]
pub mod label {
    pub const PAIR_SETUP_ENCRYPT_SALT: &[u8] = b"Pair-Setup-Encrypt-Salt";
    pub const PAIR_SETUP_ENCRYPT_INFO: &[u8] = b"Pair-Setup-Encrypt-Info";
    pub const PAIR_SETUP_CONTROLLER_SIGN_SALT: &[u8] = b"Pair-Setup-Controller-Sign-Salt";
    pub const PAIR_SETUP_CONTROLLER_SIGN_INFO: &[u8] = b"Pair-Setup-Controller-Sign-Info";
    pub const PAIR_SETUP_ACCESSORY_SIGN_SALT: &[u8] = b"Pair-Setup-Accessory-Sign-Salt";
    pub const PAIR_SETUP_ACCESSORY_SIGN_INFO: &[u8] = b"Pair-Setup-Accessory-Sign-Info";
    pub const PAIR_VERIFY_ENCRYPT_SALT: &[u8] = b"Pair-Verify-Encrypt-Salt";
    pub const PAIR_VERIFY_ENCRYPT_INFO: &[u8] = b"Pair-Verify-Encrypt-Info";

    /// Nonce labels for pairing messages (prefixed by 4 zero bytes).
    pub const PS_MSG05: &[u8; 8] = b"PS-Msg05";
    pub const PS_MSG06: &[u8; 8] = b"PS-Msg06";
    pub const PV_MSG02: &[u8; 8] = b"PV-Msg02";
    pub const PV_MSG03: &[u8; 8] = b"PV-Msg03";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Expected values computed with Python `cryptography` HKDF(SHA512, length=32).
    #[test]
    fn known_answers() {
        let ikm: Vec<u8> = (0..32).collect();
        let out: [u8; 32] = hkdf_sha512(
            label::PAIR_VERIFY_ENCRYPT_SALT,
            label::PAIR_VERIFY_ENCRYPT_INFO,
            &ikm,
        );
        assert_eq!(
            out.to_vec(),
            hex("faf9f3558a8ed1e45219bd94fb6d27e5b43a1bc861157fc2a0d291d8e3df410a")
        );
        let out: [u8; 32] = hkdf_sha512(b"", b"ClientEncrypt-main", &ikm);
        assert_eq!(
            out.to_vec(),
            hex("9f8aa265911271e6d6e90daf564d82b5913d4b760373afbd6c03e675af681f8d")
        );
    }
}
