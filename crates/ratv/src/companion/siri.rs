//! Siri over Companion, as the iPhone remote sends it.

use std::sync::atomic::Ordering;
use std::time::Duration;

use ratv_companion::message::{SiriAudio, SiriStart, SiriStop};
use tokio::time::Instant;

use super::Client;
use crate::Error;

/// Packets in one audio event: 100 ms, as the iPhone sends.
const PACKETS_PER_EVENT: usize = 5;
/// Audio length of one packet.
const PACKET: Duration = Duration::from_millis(20);

/// Options for [`Client::start_siri`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SiriOptions {
    /// Also send `_isVT = 0` and `_pttEligible = 1` with start and stop.
    /// Off by default; the device answers Siri without them.
    pub ptt_flags: bool,
}

/// Siri listening to voice input, from [`Client::start_siri`].
///
/// Audio is sent in events of 100 ms, no faster than real time.
/// After the device stops recording, see [`ended`](Self::ended),
/// more audio is not sent.
/// Siri is released when this is dropped, but call [`finish`](Self::finish)
/// to send the rest of the audio and to know whether the release failed.
/// After the device stopped recording, and audio was sent, no release is sent.
/// It holds a [`Client`] clone, so dropping the other clones does not stop the connection.
#[must_use = "Siri stays active until `finish` is called or this is dropped"]
pub struct Siri {
    client: Client,
    ptt_flags: bool,
    /// Packets that are not sent yet, one after the other, from `sent_bytes`.
    data: Vec<u8>,
    /// Sizes of the packets in `data`, from `sent_sizes`.
    sizes: Vec<u16>,
    /// Bytes of `data` that are sent.
    sent_bytes: usize,
    /// Entries of `sizes` that are sent.
    sent_sizes: usize,
    #[cfg(feature = "opus")]
    /// Boxed because it is large.
    encoder: Option<Box<ratv_mrp::VoiceEncoder>>,
    /// Packets sent.
    packets: u32,
    /// Earliest time to send the next event.
    next: Option<Instant>,
    /// True until the release is sent.
    held: bool,
    /// When [`Client::start_siri`] was called.
    begin: Instant,
    /// [`Client::siri_endpoints`] before the start, to see
    /// [`Event::SiriEndpoint`](super::Event::SiriEndpoint).
    endpoints: u32,
    /// True after [`Event::SiriEndpoint`](super::Event::SiriEndpoint).
    ended: bool,
}

impl Client {
    /// Holds Siri. Send the audio with [`Siri::send`] or `Siri::push_pcm`
    /// (feature `opus`), then release Siri with [`Siri::finish`].
    ///
    /// Fails with [`Error::SiriDisabled`] if Siri is off in the device settings,
    /// and with [`Error::SiriActive`] while another [`Siri`] exists. On a
    /// closed connection, this call fails with [`Error::Closed`]. Sending
    /// fails with it too, unless the device stopped recording before.
    pub async fn start_siri(&self, options: &SiriOptions) -> Result<Siri, Error> {
        // The last value of a closed connection would give `SiriDisabled`.
        if !self.is_connected() {
            return Err(Error::Closed);
        }
        if self.siri_enabled() == Some(false) {
            return Err(Error::SiriDisabled);
        }
        if self.siri_active().swap(true, Ordering::AcqRel) {
            return Err(Error::SiriActive);
        }
        let begin = Instant::now();
        // Releases Siri and the flag if the start fails or the future is dropped.
        let mut siri = Siri {
            client: self.clone(),
            ptt_flags: options.ptt_flags,
            data: Vec::new(),
            sizes: Vec::new(),
            sent_bytes: 0,
            sent_sizes: 0,
            #[cfg(feature = "opus")]
            encoder: None,
            packets: 0,
            next: None,
            held: true,
            begin,
            endpoints: self.siri_endpoints(),
            ended: false,
        };
        let start = self
            .request(&SiriStart {
                ptt_flags: options.ptt_flags,
            })
            .await;
        if let Err(err) = start {
            // If the start was not sent, no release is sent either.
            siri.held = !err.is_not_sent();
            return Err(err);
        }
        tracing::debug!(parent: self.span(), elapsed = ?begin.elapsed(), ptt_flags = options.ptt_flags, "Siri started");
        Ok(siri)
    }
}

impl Siri {
    /// Sends one Opus packet of 20 ms, 16 kHz, mono.
    /// Send one packet every 20 ms. A packet over 65535 bytes gives
    /// [`Error::InvalidArgument`]. On a closed connection, fails with
    /// [`Error::Closed`], unless the device stopped recording before (see
    /// [`ended`](Self::ended)).
    /// On [`Error::QueueFull`] the packet is kept and sent with the next
    /// call; do not send it again.
    pub async fn send(&mut self, packet: &[u8]) -> Result<(), Error> {
        let size = packet_size(packet)?;
        if self.ended() {
            return Ok(());
        }
        // Without this check, this returns `Ok` until enough packets for one
        // event are kept.
        if !self.client.is_connected() {
            return Err(Error::Closed);
        }
        self.data.extend_from_slice(packet);
        self.sizes.push(size);
        self.send_events(false).await
    }

    /// Sends signed 16-bit, 16 kHz, mono PCM in chunks of any size.
    /// Do not mix with [`send`](Self::send). On a closed connection, fails
    /// with [`Error::Closed`], unless the device stopped recording before.
    /// On [`Error::QueueFull`] the audio is kept and sent with the next call;
    /// do not push it again.
    #[cfg(feature = "opus")]
    pub async fn push_pcm(&mut self, pcm: &[i16]) -> Result<(), Error> {
        if self.ended() {
            return Ok(());
        }
        // Without this check, this returns `Ok` until enough packets for one
        // event are kept.
        if !self.client.is_connected() {
            return Err(Error::Closed);
        }
        let mut result = Ok(());
        let encoder = self.encoder.get_or_insert_with(Box::default);
        let (data, sizes) = (&mut self.data, &mut self.sizes);
        encoder.push(pcm, |p| {
            if result.is_ok() {
                result = keep(data, sizes, p);
            }
        })?;
        result?;
        self.send_events(false).await
    }

    /// Sends the rest of the audio and releases Siri. The device then answers.
    ///
    /// Fails with [`Error::SiriNoAudio`] if no audio was sent, for example
    /// because none was given or because the device stopped recording before
    /// audio was sent, and with [`Error::Closed`] on a closed connection. If
    /// the release gets [`Error::SendTimeout`], it is still sent later, unless
    /// the device stopped recording and audio was sent.
    ///
    /// After the device stopped recording (see [`ended`](Self::ended)), and
    /// audio was sent, no release is sent and this does not fail with
    /// [`Error::Closed`]. On tvOS 27.2.1, after a release at that point the
    /// screen stayed dimmed and no remote worked until the device ended the
    /// session after some minutes.
    /// Apple's Siri Remote does the same when it is held past the end of
    /// listening.
    ///
    /// Keep the connection open until the device has answered, for example
    /// for 20 s: the answer can take some seconds. On tvOS 27.2.1, when the
    /// connection closed a few seconds after `finish`, Siri did not answer.
    pub async fn finish(mut self) -> Result<(), Error> {
        #[cfg(feature = "opus")]
        if let Some(encoder) = &mut self.encoder {
            let mut result = Ok(());
            let (data, sizes) = (&mut self.data, &mut self.sizes);
            encoder.flush(|p| {
                if result.is_ok() {
                    result = keep(data, sizes, p);
                }
            })?;
            result?;
        }
        self.send_events(true).await?;
        self.held = false;
        if self.release_skipped() {
            tracing::debug!(
                parent: self.client.span(),
                elapsed = ?self.begin.elapsed(),
                packets = self.packets,
                "Siri ended by the device, no release sent"
            );
            return Ok(());
        }
        let stop = SiriStop {
            ptt_flags: self.ptt_flags,
        };
        let result = self.client.release_request(&stop).await.map(drop);
        // Not sent in time: the drop sends it without a timeout.
        self.held = matches!(result, Err(Error::SendTimeout(_)));
        tracing::debug!(
            parent: self.client.span(),
            elapsed = ?self.begin.elapsed(),
            packets = self.packets,
            "Siri released"
        );
        if self.packets == 0 {
            return result.and(Err(Error::SiriNoAudio));
        }
        result
    }

    /// True after the device stopped recording. Call
    /// [`finish`](Self::finish) then; audio given after this is not sent.
    ///
    /// With push-to-talk, tvOS 26 stops recording only after [`finish`](Self::finish).
    pub fn ended(&mut self) -> bool {
        if !self.ended && self.client.siri_endpoints() != self.endpoints {
            self.ended = true;
            tracing::debug!(parent: self.client.span(), elapsed = ?self.begin.elapsed(), packets = self.packets, "Siri endpoint");
        }
        self.ended
    }

    /// True if no release is sent: the device stopped recording, and audio
    /// was sent.
    /// On tvOS 27.2.1 a `_siriStop` after `_siriEndpoint` left the screen
    /// dimmed and no remote worked. A stop sent before the endpoint arrives
    /// can still reach the device after it. Without audio, the endpoint is
    /// most likely from an earlier session, so the release is sent.
    fn release_skipped(&mut self) -> bool {
        self.packets > 0 && self.ended()
    }

    /// Sends full events, and with `all` also the last part.
    async fn send_events(&mut self, all: bool) -> Result<(), Error> {
        loop {
            let left = self.sizes.len() - self.sent_sizes;
            if left == 0 || self.ended() {
                self.data.clear();
                self.sizes.clear();
                self.sent_bytes = 0;
                self.sent_sizes = 0;
                return Ok(());
            }
            if left < PACKETS_PER_EVENT && !all {
                // Moves the few packets left to the front, so `data` does not grow.
                self.data.drain(..self.sent_bytes);
                self.sizes.drain(..self.sent_sizes);
                self.sent_bytes = 0;
                self.sent_sizes = 0;
                return Ok(());
            }
            let n = left.min(PACKETS_PER_EVENT);
            let now = Instant::now();
            // A late event does not make later events go faster.
            let at = self.next.map_or(now, |next| next.max(now));
            tokio::time::sleep_until(at).await;
            if self.ended() {
                continue;
            }
            let sizes = &self.sizes[self.sent_sizes..self.sent_sizes + n];
            let len: usize = sizes.iter().map(|&s| usize::from(s)).sum();
            // `n` is at most 5.
            #[allow(clippy::cast_possible_truncation)]
            let count = n as u32;
            let data = &self.data[self.sent_bytes..self.sent_bytes + len];
            self.client.send_event(&SiriAudio { data, sizes }).await?;
            // Only after a send, so a failed send does not delay the next one.
            self.next = Some(at + PACKET * count);
            if self.packets == 0 {
                tracing::debug!(parent: self.client.span(), elapsed = ?self.begin.elapsed(), "Siri first audio");
            }
            // Sent packets are not sent again.
            self.sent_bytes += len;
            self.sent_sizes += n;
            self.packets += count;
        }
    }
}

/// Size of one packet, which must fit in a `u16`.
fn packet_size(packet: &[u8]) -> Result<u16, Error> {
    u16::try_from(packet.len()).map_err(|_| Error::InvalidArgument("packet size"))
}

/// Adds a packet to the ones that are not sent yet.
#[cfg(feature = "opus")]
fn keep(data: &mut Vec<u8>, sizes: &mut Vec<u16>, packet: &[u8]) -> Result<(), Error> {
    let size = packet_size(packet)?;
    data.extend_from_slice(packet);
    sizes.push(size);
    Ok(())
}

impl Drop for Siri {
    fn drop(&mut self) {
        if self.held && !self.release_skipped() {
            tracing::debug!(parent: self.client.span(), "Siri released on drop");
            let stop = SiriStop {
                ptt_flags: self.ptt_flags,
            };
            let _ = self.client.queue_event(&stop, true);
        }
        self.client.siri_active().store(false, Ordering::Release);
    }
}
