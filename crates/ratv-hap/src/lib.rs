//! HAP (HomeKit Accessory Protocol) pairing and session encryption.
//!
//! Apple TV uses HAP pairing for both the Companion protocol and AirPlay 2.
//! Everything here is sans-IO: functions take received bytes and return bytes
//! to send. The caller wraps them in the transport of each protocol.
//!
//! - [`pair_setup`]: first-time pairing with a PIN (or transient pairing).
//! - [`pair_verify`]: per-connection key agreement using stored [`Credentials`].
//! - [`cipher`]: ChaCha20-Poly1305 frame encryption with per-direction counters.

pub mod cipher;
mod credentials;
mod error;
pub mod kdf;
pub mod pair_setup;
pub mod pair_verify;
mod secret;
pub mod srp;

#[cfg(any(test, feature = "accessory"))]
pub mod accessory;

pub use credentials::{Credentials, ParseCredentialsError};
pub use error::{DeviceError, Error};
pub use secrecy::{ExposeSecret, SecretString};
pub use secret::SharedSecret;

/// Random number generator accepted by this crate.
pub use rand_core::CryptoRng;

/// Values used for the `METHOD` TLV item.
pub(crate) mod method {
    pub(crate) const PAIR_SETUP: u8 = 0x00;
}

/// Value of the `FLAGS` TLV item for transient pairing.
pub(crate) const FLAG_TRANSIENT: u8 = 0x10;

/// Reads a TLV message and fails if it carries an error item or an unexpected state.
pub(crate) fn expect_state(msg: &[u8], state: u8) -> Result<ratv_codec::tlv8::Tlv8<'_>, Error> {
    use ratv_codec::tlv8::{Tlv8, tag};
    let tlv = Tlv8::decode(msg)?;
    if let Some(code) = tlv.get_u8(tag::ERROR) {
        let backoff = tlv.get(tag::BACKOFF).map(|b| {
            let mut buf = [0u8; 2];
            let n = b.len().min(2);
            buf[..n].copy_from_slice(&b[..n]);
            u16::from_le_bytes(buf)
        });
        return Err(Error::Device(DeviceError::from_code(code, backoff)));
    }
    match tlv.get_u8(tag::STATE) {
        Some(s) if s == state => Ok(tlv),
        got => Err(Error::UnexpectedState {
            expected: state,
            got,
        }),
    }
}

/// Returns a required TLV item.
pub(crate) fn required<'t>(tlv: &'t ratv_codec::tlv8::Tlv8<'_>, ty: u8) -> Result<&'t [u8], Error> {
    tlv.get(ty).ok_or(Error::MissingField(ty))
}

/// Returns a required 32-byte TLV item.
pub(crate) fn required_32(tlv: &ratv_codec::tlv8::Tlv8<'_>, ty: u8) -> Result<[u8; 32], Error> {
    required(tlv, ty)?
        .try_into()
        .map_err(|_| Error::InvalidLength(ty))
}

pub(crate) fn random_32(rng: &mut impl CryptoRng) -> [u8; 32] {
    let mut buf = [0u8; 32];
    rng.fill_bytes(&mut buf);
    buf
}
