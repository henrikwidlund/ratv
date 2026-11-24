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
    /// A value is outside its range, for example a volume of NaN, or the
    /// current item does not list it, for example a playback rate.
    #[error("invalid {0}")]
    InvalidArgument(&'static str),
    /// No text field has focus on the device.
    #[error("no text field has focus")]
    NoTextField,
    /// Another Siri session is active.
    #[error("Siri is already active")]
    SiriActive,
    /// Siri is off in the device settings.
    #[error("Siri is off on the device")]
    SiriDisabled,
    /// The device did not start recording voice input.
    /// Siri may be off in the device settings.
    #[error("Siri did not start recording")]
    SiriNotRecording,
    /// Siri was released without audio.
    #[error("no audio was sent to Siri")]
    SiriNoAudio,
    /// The device cannot change the volume, for example with HDMI audio without CEC.
    #[error("volume control is not available")]
    VolumeNotAvailable,
    /// The item has no next or previous chapter.
    #[error("no such chapter")]
    NoChapter,
    /// Connecting, a handshake step or a write did not finish in time.
    #[error("timed out")]
    Timeout,
    /// The device did not answer this request in time. Also the reason from
    /// `closed` when the device stopped answering: Companion requests or MRP
    /// feedback.
    #[error("no answer to {0}")]
    NoAnswer(&'static str),
    /// The request was not written to the network in time, for example
    /// because the device does not read. It was removed, so it is not sent.
    /// The release of `press`, `hold` and `Siri::finish` is queued again, see
    /// [`Error::is_not_sent`].
    #[error("{0} was not sent in time")]
    SendTimeout(&'static str),
    /// The connection closed while a call or a connect step waited. The request
    /// may have been sent. Also the reason from `closed` when the device closed
    /// the connection.
    #[error("connection closed")]
    Disconnected,
    /// The connection was closed before this message was queued or before it
    /// was taken from the outbox, so it was not sent. Earlier messages of the
    /// same call may have been sent.
    #[error("connection is closed, not sent")]
    Closed,
    /// Too much data waits to be sent. The device does not read it.
    /// This message was not sent. Earlier messages of the same call may have
    /// been sent.
    #[error("send queue is full")]
    QueueFull,
    /// Device sent something unexpected.
    #[error("protocol error: {0}")]
    Protocol(&'static str),
    /// A Companion pairing step failed while waiting for the reply.
    /// Network errors are [`Error::Io`] instead.
    #[cfg(feature = "companion")]
    #[error("{step}: {source}{}", Skipped(.skipped))]
    PairingStep {
        /// The reply we waited for, for example `pair-setup M2`.
        step: &'static str,
        /// Type and payload length of the first frames skipped while waiting.
        skipped: Vec<(u8, usize)>,
        /// The cause.
        source: Box<Self>,
    },
}

/// Shows skipped frames, or nothing if there are none.
#[cfg(feature = "companion")]
struct Skipped<'a>(&'a [(u8, usize)]);

#[cfg(feature = "companion")]
impl core::fmt::Display for Skipped<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, (ty, len)) in self.0.iter().enumerate() {
            let sep = if i == 0 { " (skipped frames: " } else { "; " };
            write!(f, "{sep}type {ty}, {len} bytes")?;
        }
        if self.0.is_empty() {
            Ok(())
        } else {
            f.write_str(")")
        }
    }
}

impl From<tokio::time::error::Elapsed> for Error {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}

impl Error {
    /// True if a connect step got no answer in time.
    #[cfg(any(feature = "companion", feature = "mrp"))]
    #[cfg_attr(not(feature = "companion"), allow(clippy::missing_const_for_fn))]
    pub(crate) fn is_stall(&self) -> bool {
        match self {
            Self::Timeout => true,
            #[cfg(feature = "companion")]
            Self::PairingStep { source, .. } => matches!(**source, Self::Timeout),
            _ => false,
        }
    }

    /// True if the connection is closed; connect again. True for
    /// [`Error::Closed`] and [`Error::Disconnected`].
    ///
    /// With [`AppleTv`](crate::AppleTv), use
    /// [`connect_missing`](crate::AppleTv::connect_missing). It also adds a
    /// protocol that did not connect ([`Error::NotConnected`]).
    ///
    /// For the error of a call, not for the error from `closed`.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        matches!(self, Self::Closed | Self::Disconnected)
    }

    /// True if the device refused the TCP connection. The service may listen
    /// on another port now, so scan again to find it. Other connect errors,
    /// for example [`Error::Timeout`], can mean that the device is still starting.
    #[must_use]
    pub fn is_refused(&self) -> bool {
        matches!(self, Self::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused)
    }

    /// True if the device got a request and answered with an error, for
    /// example because the app has no handler for a command. Companion connect
    /// also returns it if the device rejects the system info or session start
    /// request, and MRP connect and pairing if the device answers an RTSP
    /// request with an error status. Other pairing errors are not included.
    #[must_use]
    pub const fn is_rejected(&self) -> bool {
        match self {
            #[cfg(feature = "companion")]
            Self::Companion(ratv_companion::Error::Device { .. }) => true,
            #[cfg(feature = "mrp")]
            Self::Mrp(ratv_mrp::Error::Device { .. })
            | Self::CommandFailed { .. }
            | Self::Airplay(ratv_airplay::Error::Status(_)) => true,
            _ => false,
        }
    }

    /// True if a Companion message was too large. From a call on an open
    /// connection: our message, for example a very long text or URL. Nothing was
    /// sent and the connection stays open. From `closed`: a frame from the device.
    #[must_use]
    pub const fn is_too_large(&self) -> bool {
        match self {
            #[cfg(feature = "companion")]
            Self::Companion(ratv_companion::Error::FrameTooLarge(_)) => true,
            _ => false,
        }
    }

    /// True if the device rejected the credentials, or the PIN while pairing.
    /// After a failed connect, pair again to fix it.
    ///
    /// A device with another identifier is [`is_other_device`](Self::is_other_device) instead.
    #[must_use]
    pub const fn is_auth(&self) -> bool {
        matches!(
            self.hap(),
            Some(
                ratv_hap::Error::BadSignature
                    | ratv_hap::Error::BadProof
                    | ratv_hap::Error::Device(ratv_hap::DeviceError::Authentication)
            )
        )
    }

    /// True if pair-verify found a device with another identifier than the
    /// credentials. Usually another device now has the address, for example
    /// after the router gave the address to a new device. Scan again to find
    /// the paired device. Pairing again with this address pairs the other device.
    ///
    /// The identifier is checked before the signature, so a wrong device always
    /// gives this error, not [`is_auth`](Self::is_auth).
    #[must_use]
    pub const fn is_other_device(&self) -> bool {
        matches!(self.hap(), Some(ratv_hap::Error::UnknownAccessory))
    }

    /// The pairing error inside `self`, also inside `PairingStep`, if any.
    const fn hap(&self) -> Option<&ratv_hap::Error> {
        match self {
            Self::Hap(e) => Some(e),
            #[cfg(feature = "companion")]
            Self::Companion(ratv_companion::Error::Hap(e)) => Some(e),
            #[cfg(feature = "companion")]
            Self::PairingStep { source, .. } => source.hap(),
            #[cfg(feature = "mrp")]
            Self::Airplay(ratv_airplay::Error::Hap(e)) => Some(e),
            _ => None,
        }
    }

    /// True if the request was not sent, so it is safe to send it again,
    /// for example after a reconnect. True for [`Error::Closed`],
    /// [`Error::SendTimeout`], [`Error::QueueFull`], [`Error::NotConnected`],
    /// [`Error::NoEndpoints`], [`Error::InvalidArgument`],
    /// [`Error::VolumeNotAvailable`], [`Error::NoChapter`],
    /// [`Error::NoTextField`], [`Error::SiriActive`], [`Error::SiriDisabled`],
    /// and for a Companion message that is too large (see
    /// [`is_too_large`](Self::is_too_large)).
    ///
    /// False does not mean that the device got the request. For example
    /// [`Error::Disconnected`] and [`Error::NoAnswer`] can come before or after
    /// the device got it.
    ///
    /// A call that sends more than one message can fail after it sent some of
    /// them. Then only the failing message was not sent. Examples: `press`
    /// (button down, then up), `hold`, play and pause when they try another
    /// command or protocol after the first one failed, and the Companion text
    /// calls, which restart the text session before they fail with
    /// [`Error::NoTextField`].
    ///
    /// If the press of `press`, `hold`, Siri, or Companion `tap` and `swipe`
    /// was not sent, the release is not sent either. The release of `press`,
    /// `hold` and `Siri::finish` that fails with [`Error::SendTimeout`] is still
    /// sent later, without a time limit, unless the connection closes first.
    /// So the device does not keep the button or Siri held.
    ///
    /// `double_tap` is not covered: when it fails, some of its 4 messages can
    /// still be sent.
    ///
    /// Not for the error from `closed`. Of the connect errors, only
    /// [`Error::NoEndpoints`] is covered.
    #[must_use]
    pub const fn is_not_sent(&self) -> bool {
        match self {
            Self::Closed
            | Self::SendTimeout(_)
            | Self::QueueFull
            | Self::NotConnected(_)
            | Self::NoEndpoints
            | Self::InvalidArgument(_)
            | Self::VolumeNotAvailable
            | Self::NoChapter
            | Self::NoTextField
            | Self::SiriActive
            | Self::SiriDisabled => true,
            #[cfg(feature = "companion")]
            Self::Companion(ratv_companion::Error::FrameTooLarge(_)) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_auth() {
        assert!(!Error::Hap(ratv_hap::Error::UnknownAccessory).is_auth());
        assert!(Error::Hap(ratv_hap::Error::BadSignature).is_auth());
        assert!(
            Error::Hap(ratv_hap::Error::Device(
                ratv_hap::DeviceError::Authentication
            ))
            .is_auth()
        );
        assert!(!Error::Hap(ratv_hap::Error::Decrypt).is_auth());
        assert!(!Error::Timeout.is_auth());
        #[cfg(feature = "mrp")]
        assert!(Error::Airplay(ratv_airplay::Error::Hap(ratv_hap::Error::BadProof)).is_auth());
        #[cfg(feature = "companion")]
        {
            let step = |source| Error::PairingStep {
                step: "pair-verify M4",
                skipped: Vec::new(),
                source: Box::new(source),
            };
            assert!(step(Error::Hap(ratv_hap::Error::BadSignature)).is_auth());
            assert!(!step(Error::Timeout).is_auth());
        }
    }

    #[test]
    fn is_other_device() {
        let unknown = || ratv_hap::Error::UnknownAccessory;
        assert!(Error::Hap(unknown()).is_other_device());
        assert!(!Error::Hap(ratv_hap::Error::BadSignature).is_other_device());
        assert!(!Error::Timeout.is_other_device());
        #[cfg(feature = "companion")]
        {
            assert!(Error::Companion(ratv_companion::Error::Hap(unknown())).is_other_device());
            let step = Error::PairingStep {
                step: "pair-verify M2",
                skipped: Vec::new(),
                source: Box::new(Error::Hap(unknown())),
            };
            assert!(step.is_other_device());
            assert!(!step.is_auth());
        }
        #[cfg(feature = "mrp")]
        assert!(Error::Airplay(ratv_airplay::Error::Hap(unknown())).is_other_device());
    }

    #[test]
    fn is_not_sent() {
        for err in [
            Error::Closed,
            Error::SendTimeout("x"),
            Error::QueueFull,
            Error::NotConnected("MRP"),
            Error::NoEndpoints,
            Error::InvalidArgument("volume"),
            Error::VolumeNotAvailable,
            Error::NoChapter,
            Error::NoTextField,
            Error::SiriActive,
            Error::SiriDisabled,
        ] {
            assert!(err.is_not_sent(), "{err}");
        }
        for err in [
            Error::Disconnected,
            Error::NoAnswer("x"),
            Error::Timeout,
            Error::Protocol("x"),
        ] {
            assert!(!err.is_not_sent(), "{err}");
        }
        #[cfg(feature = "companion")]
        assert!(Error::Companion(ratv_companion::Error::FrameTooLarge(1)).is_not_sent());
    }

    #[test]
    fn is_rejected() {
        assert!(!Error::Disconnected.is_rejected());
        assert!(!Error::NoAnswer("x").is_rejected());
        #[cfg(feature = "companion")]
        {
            let device = ratv_companion::Error::Device {
                code: 1,
                domain: String::new(),
                message: String::new(),
            };
            assert!(Error::Companion(device).is_rejected());
            assert!(!Error::Companion(ratv_companion::Error::FrameTooLarge(1)).is_rejected());
        }
        #[cfg(feature = "mrp")]
        {
            let device = ratv_mrp::Error::Device {
                code: 1,
                description: String::new(),
            };
            assert!(Error::Mrp(device).is_rejected());
            let failed = Error::CommandFailed {
                send_error: 0,
                handler_status: 404,
            };
            assert!(failed.is_rejected());
            assert!(Error::Airplay(ratv_airplay::Error::Status(403)).is_rejected());
        }
    }

    #[test]
    fn is_closed() {
        assert!(Error::Closed.is_closed());
        assert!(Error::Disconnected.is_closed());
        assert!(!Error::SendTimeout("x").is_closed());
        assert!(!Error::NoAnswer("x").is_closed());
        assert!(!Error::Timeout.is_closed());
    }

    #[test]
    fn is_too_large() {
        assert!(!Error::QueueFull.is_too_large());
        #[cfg(feature = "companion")]
        assert!(Error::Companion(ratv_companion::Error::FrameTooLarge(1)).is_too_large());
    }

    #[test]
    fn is_refused() {
        let io = |kind| Error::Io(std::io::Error::from(kind));
        assert!(io(std::io::ErrorKind::ConnectionRefused).is_refused());
        assert!(!io(std::io::ErrorKind::TimedOut).is_refused());
        assert!(!Error::Timeout.is_refused());
    }

    #[test]
    #[cfg(any(feature = "companion", feature = "mrp"))]
    fn is_stall() {
        assert!(Error::Timeout.is_stall());
        assert!(!Error::Disconnected.is_stall());
        #[cfg(feature = "companion")]
        {
            let step = |source| Error::PairingStep {
                step: "pair-verify M2",
                skipped: Vec::new(),
                source: Box::new(source),
            };
            assert!(step(Error::Timeout).is_stall());
            assert!(!step(Error::Disconnected).is_stall());
        }
    }
}
