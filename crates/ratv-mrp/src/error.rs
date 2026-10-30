/// MRP errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The protobuf data is invalid.
    #[error("invalid protobuf: {0}")]
    Decode(#[from] prost::DecodeError),
    /// A length or field goes past the end of the data.
    #[error("message is truncated")]
    Truncated,
    /// The data uses a protobuf feature MRP does not use (groups).
    #[error("unsupported wire type {0}")]
    WireType(u64),
    /// A header string is not UTF-8.
    #[error("header field is not UTF-8")]
    Utf8,
    /// The message has a different payload than requested.
    #[error("expected payload field {expected}, got {actual:?}")]
    UnexpectedPayload {
        /// Requested field number.
        expected: u32,
        /// Field number in the message.
        actual: Option<u32>,
    },
    /// The device returned an error code.
    #[error("device error {code}: {description}")]
    Device {
        /// `ErrorCode` value.
        code: i32,
        /// Error text, may be empty.
        description: String,
    },
}
