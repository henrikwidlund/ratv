/// Error from a device connection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Network error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Pairing or encryption error.
    #[error(transparent)]
    Hap(#[from] ratv_hap::Error),
    /// Companion protocol error.
    #[cfg(feature = "companion")]
    #[error(transparent)]
    Companion(#[from] ratv_companion::Error),
    /// AirPlay transport error.
    #[cfg(feature = "mrp")]
    #[error(transparent)]
    Airplay(#[from] ratv_airplay::Error),
    /// MRP error.
    #[cfg(feature = "mrp")]
    #[error(transparent)]
    Mrp(#[from] ratv_mrp::Error),
    /// The device did not run an MRP command.
    #[cfg(feature = "mrp")]
    #[error("command failed: send error {send_error}, handler status {handler_status}")]
    CommandFailed {
        /// `SendError` value.
        send_error: i32,
        /// `HandlerReturnStatus` value.
        handler_status: i32,
    },
    /// The protocol needed for this call is not connected.
    #[error("{0} is not connected")]
    NotConnected(&'static str),
    /// No address to connect to was given.
    #[error("no endpoints")]
    NoEndpoints,
    /// A value is outside its range, for example a volume of NaN.
    #[error("invalid {0}")]
    InvalidArgument(&'static str),
    /// No text field has focus on the device.
    #[error("no text field has focus")]
    NoTextField,
    /// Another Siri session is active.
    #[error("Siri is already active")]
    SiriActive,
    /// The device did not start recording voice input.
    #[error("Siri did not start recording")]
    SiriNotRecording,
    /// The item has no next or previous chapter.
    #[error("no such chapter")]
    NoChapter,
    /// Connecting, a handshake step or a write did not finish in time.
    #[error("timed out")]
    Timeout,
    /// The device did not answer this request in time.
    #[error("no answer to {0}")]
    NoAnswer(&'static str),
    /// Connection is closed.
    #[error("connection closed")]
    Disconnected,
    /// Too much data waits to be sent. The device does not read it.
    #[error("send queue is full")]
    QueueFull,
    /// Device sent something unexpected.
    #[error("protocol error: {0}")]
    Protocol(&'static str),
}

impl From<tokio::time::error::Elapsed> for Error {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}
