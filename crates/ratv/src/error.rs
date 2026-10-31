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
    /// The device cannot change the volume, for example with HDMI audio without CEC.
    #[error("volume control is not available")]
    VolumeNotAvailable,
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

impl Error {
    /// True if the device rejected the credentials, or the PIN while pairing.
    /// After a failed connect, pair again to fix it.
    #[must_use]
    pub const fn is_auth(&self) -> bool {
        match self {
            Self::Hap(e) => hap_auth(e),
            #[cfg(feature = "companion")]
            Self::Companion(ratv_companion::Error::Hap(e)) => hap_auth(e),
            #[cfg(feature = "mrp")]
            Self::Airplay(ratv_airplay::Error::Hap(e)) => hap_auth(e),
            _ => false,
        }
    }
}

const fn hap_auth(e: &ratv_hap::Error) -> bool {
    matches!(
        e,
        ratv_hap::Error::UnknownAccessory
            | ratv_hap::Error::BadSignature
            | ratv_hap::Error::BadProof
            | ratv_hap::Error::Device(ratv_hap::DeviceError::Authentication)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_auth() {
        assert!(Error::Hap(ratv_hap::Error::UnknownAccessory).is_auth());
        assert!(
            Error::Hap(ratv_hap::Error::Device(
                ratv_hap::DeviceError::Authentication
            ))
            .is_auth()
        );
        assert!(!Error::Hap(ratv_hap::Error::Decrypt).is_auth());
        assert!(!Error::Timeout.is_auth());
    }
}
