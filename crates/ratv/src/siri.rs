//! Siri over Companion or MRP, from [`AppleTv::start_siri`](crate::AppleTv::start_siri).

use bytes::Bytes;

use crate::{Error, companion, mrp};

/// Siri listening to voice input, from [`AppleTv::start_siri`](crate::AppleTv::start_siri).
///
/// Siri is released when this is dropped, but call [`finish`](Self::finish)
/// to send the rest of the audio and to know whether the release failed.
/// Over Companion, no release is sent after the device stopped recording, if
/// audio was sent.
/// It holds a client clone, so dropping the other clones does not stop the connection.
#[must_use = "Siri stays active until `finish` is called or this is dropped"]
pub enum Siri {
    /// Over Companion, as the iPhone remote sends it.
    Companion(companion::Siri),
    /// Over MRP. Some devices with tvOS 27 record but do not hear this audio.
    Mrp(mrp::Siri),
}

impl Siri {
    /// Sends one Opus packet of 20 ms, 16 kHz, mono.
    /// Send one packet every 20 ms. A packet over 65535 bytes gives
    /// [`Error::InvalidArgument`]. On a closed connection, fails with
    /// [`Error::Closed`], unless the device stopped recording before (see
    /// [`ended`](Self::ended)). Over MRP, fails with
    /// [`Error::SiriNotRecording`] after 30 s of audio kept while the device
    /// does not record.
    /// On [`Error::QueueFull`] the packet is kept and sent with the next
    /// call; do not send it again.
    pub async fn send(&mut self, packet: impl Into<Bytes>) -> Result<(), Error> {
        match self {
            Self::Companion(s) => s.send(&packet.into()).await,
            Self::Mrp(s) => s.send(packet).await,
        }
    }

    /// Sends signed 16-bit, 16 kHz, mono PCM in chunks of any size.
    /// Do not mix with [`send`](Self::send). Fails with [`Error::Closed`]
    /// and, over MRP, [`Error::SiriNotRecording`] as [`send`](Self::send)
    /// does.
    /// On [`Error::QueueFull`] the audio is kept and sent with the next call;
    /// do not push it again.
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
    /// Fails with [`Error::SiriNoAudio`] if no audio was sent, for example
    /// because none was given or because the device stopped recording before
    /// audio was sent, and with [`Error::Closed`] on a closed connection. Over
    /// MRP, waits for the device to record first; fails with
    /// [`Error::SiriNotRecording`] if it does not start in time. Over
    /// Companion, after the device stopped recording (see
    /// [`ended`](Self::ended)), and audio was sent, no release is sent and
    /// this does not fail with [`Error::Closed`], see
    /// [`companion::Siri::finish`](crate::companion::Siri::finish).
    ///
    /// Over Companion, keep the connection open until the device has answered,
    /// for example for 20 s: the answer can take some seconds. On tvOS 27.2.1,
    /// when the connection closed a few seconds after `finish`, Siri did not
    /// answer. Over MRP this was not tested.
    pub async fn finish(self) -> Result<(), Error> {
        match self {
            Self::Companion(s) => s.finish().await,
            Self::Mrp(s) => s.finish().await,
        }
    }
}
