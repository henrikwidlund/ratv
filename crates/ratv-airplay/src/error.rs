/// AirPlay transport errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// HTTP or RTSP message is malformed.
    #[error("invalid HTTP message: {0}")]
    InvalidMessage(&'static str),
    /// Message head is larger than [`http::MAX_HEAD_LEN`](crate::http::MAX_HEAD_LEN).
    #[error("message head too large")]
    HeadTooLarge,
    /// Body or frame is larger than the allowed size.
    #[error("message too large: {0} bytes")]
    TooLarge(usize),
    /// Decryption failed.
    #[error(transparent)]
    Hap(#[from] ratv_hap::Error),
    /// Binary plist is invalid.
    #[error("invalid plist: {0}")]
    Plist(#[from] ratv_codec::bplist::Error),
    /// The device answered with an error status.
    #[error("device returned status {0}")]
    Status(u16),
    /// A required field is missing in a response.
    #[error("missing field {0}")]
    MissingField(&'static str),
}
