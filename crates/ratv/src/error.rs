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
    /// No text field has focus on the device.
    #[error("no text field has focus")]
    NoTextField,
    /// The item has no next or previous chapter.
    #[error("no such chapter")]
    NoChapter,
    /// Device did not answer in time.
    #[error("request timed out")]
    Timeout,
    /// Connection is closed.
    #[error("connection closed")]
    Disconnected,
    /// Device sent something unexpected.
    #[error("protocol error: {0}")]
    Protocol(&'static str),
}

impl From<tokio::time::error::Elapsed> for Error {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}
