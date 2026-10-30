/// Companion protocol error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Frame is larger than the configured limit.
    #[error("frame of {0} bytes exceeds limit")]
    FrameTooLarge(usize),
    /// Encrypted frame is shorter than the authentication tag.
    #[error("encrypted frame too short")]
    FrameTooShort,
    /// Payload is not valid OPACK.
    #[error(transparent)]
    Opack(#[from] ratv_codec::opack::Error),
    /// Pairing or decryption error.
    #[error(transparent)]
    Hap(#[from] ratv_hap::Error),
    /// Message lacks a required field or has the wrong type.
    #[error("missing or invalid field `{0}`")]
    InvalidField(&'static str),
    /// Device answered a request with an error.
    #[error("device error {code}: {message}")]
    Device {
        /// Error code (`_ec`).
        code: i64,
        /// Error domain (`_ed`).
        domain: String,
        /// Error message (`_em`).
        message: String,
    },
}
