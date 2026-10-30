/// Error reported by the device in a TLV `ERROR` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeviceError {
    /// Generic error.
    #[error("unknown device error")]
    Unknown,
    /// Wrong PIN or failed proof.
    #[error("authentication failed")]
    Authentication,
    /// Too many attempts; retry after the given number of seconds.
    #[error("device asks to retry later ({0:?} s)")]
    Backoff(Option<u16>),
    /// Device cannot accept more paired controllers.
    #[error("maximum number of peers reached")]
    MaxPeers,
    /// Too many failed attempts.
    #[error("maximum number of tries reached")]
    MaxTries,
    /// Pairing is not available right now.
    #[error("pairing unavailable")]
    Unavailable,
    /// Device is busy with another pairing.
    #[error("device busy")]
    Busy,
    /// Code not known to this library.
    #[error("device error code {0}")]
    Other(u8),
}

impl DeviceError {
    pub(crate) const fn from_code(code: u8, backoff: Option<u16>) -> Self {
        match code {
            1 => Self::Unknown,
            2 => Self::Authentication,
            3 => Self::Backoff(backoff),
            4 => Self::MaxPeers,
            5 => Self::MaxTries,
            6 => Self::Unavailable,
            7 => Self::Busy,
            c => Self::Other(c),
        }
    }

    /// Returns the TLV error code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Unknown => 1,
            Self::Authentication => 2,
            Self::Backoff(_) => 3,
            Self::MaxPeers => 4,
            Self::MaxTries => 5,
            Self::Unavailable => 6,
            Self::Busy => 7,
            Self::Other(c) => c,
        }
    }
}

/// Pairing or encryption error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Message is not valid TLV8.
    #[error(transparent)]
    Tlv(#[from] ratv_codec::tlv8::Error),
    /// Device returned an error.
    #[error("device error: {0}")]
    Device(DeviceError),
    /// Message has the wrong pairing state.
    #[error("expected state {expected}, got {got:?}")]
    UnexpectedState {
        /// Expected state number.
        expected: u8,
        /// Received state number, if any.
        got: Option<u8>,
    },
    /// Required TLV item is missing.
    #[error("missing TLV item 0x{0:02x}")]
    MissingField(u8),
    /// TLV item has the wrong length.
    #[error("TLV item 0x{0:02x} has wrong length")]
    InvalidLength(u8),
    /// SRP parameters from the device are invalid.
    #[error("invalid SRP parameters")]
    InvalidSrpParameters,
    /// Device SRP proof does not match. Usually caused by a wrong PIN.
    #[error("device proof mismatch")]
    BadProof,
    /// Ed25519 signature check failed.
    #[error("signature verification failed")]
    BadSignature,
    /// Public key is not a valid curve point.
    #[error("invalid public key")]
    InvalidPublicKey,
    /// Device identifier does not match the stored credentials.
    #[error("unknown device identifier")]
    UnknownAccessory,
    /// Authenticated decryption failed.
    #[error("decryption failed")]
    Decrypt,
}
