//! Siri over Companion or MRP, from [`AppleTv::start_siri`](crate::AppleTv::start_siri).

use bytes::Bytes;

use crate::{Error, companion, mrp};

/// Siri listening to voice input, from [`AppleTv::start_siri`](crate::AppleTv::start_siri).
///
/// Siri is released when this is dropped, but call [`finish`](Self::finish)
/// to send the rest of the audio and to know that the release was sent.
#[must_use = "Siri stays active until `finish` is called or this is dropped"]
pub enum Siri {
    /// Over Companion, as the iPhone remote sends it.
    Companion(companion::Siri),
    /// Over MRP. Some devices with tvOS 27 record but do not hear this audio.
    Mrp(mrp::Siri),
}

impl Siri {
    /// Sends one Opus packet of 20 ms, 16 kHz, mono.
    /// Send one packet every 20 ms.
    pub async fn send(&mut self, packet: impl Into<Bytes>) -> Result<(), Error> {
        match self {
            Self::Companion(s) => s.send(&packet.into()).await,
            Self::Mrp(s) => s.send(packet).await,
        }
    }

    /// Sends signed 16-bit, 16 kHz, mono PCM in chunks of any size.
    /// Do not mix with [`send`](Self::send).
    #[cfg(feature = "opus")]
    pub async fn push_pcm(&mut self, pcm: &[i16]) -> Result<(), Error> {
        match self {
            Self::Companion(s) => s.push_pcm(pcm).await,
            Self::Mrp(s) => s.push_pcm(pcm).await,
        }
    }

    /// True after the device stopped recording. Call
    /// [`finish`](Self::finish) then. Always false for MRP.
    pub fn ended(&mut self) -> bool {
        match self {
            Self::Companion(s) => s.ended(),
            Self::Mrp(_) => false,
        }
    }

    /// Sends the rest of the audio and releases Siri. The device then answers.
    ///
    /// Fails with [`Error::SiriNoAudio`] if no audio was given.
    pub async fn finish(self) -> Result<(), Error> {
        match self {
            Self::Companion(s) => s.finish().await,
            Self::Mrp(s) => s.finish().await,
        }
    }
}
