use zeroize::Zeroizing;

use crate::kdf;

/// Shared secret from pair-verify (X25519, 32 bytes) or transient pair-setup (SRP, 64 bytes).
///
/// The memory is cleared on drop. Use [`SharedSecret::derive`] to get session keys.
#[derive(Clone)]
pub struct SharedSecret(Zeroizing<Vec<u8>>);

impl SharedSecret {
    pub(crate) fn new(bytes: &[u8]) -> Self {
        Self(Zeroizing::new(bytes.to_vec()))
    }

    /// Creates a secret from raw bytes. For tests and fake devices.
    #[cfg(any(test, feature = "accessory"))]
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self::new(bytes)
    }

    /// Derives a 32-byte key with HKDF-SHA512.
    #[must_use]
    pub fn derive(&self, salt: &[u8], info: &[u8]) -> [u8; 32] {
        kdf::hkdf_sha512(salt, info, &self.0)
    }

    /// Raw secret bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl core::fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SharedSecret(..)")
    }
}
