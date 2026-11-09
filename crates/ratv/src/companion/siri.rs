//! Siri over Companion, as the iPhone remote sends it.

use std::sync::atomic::Ordering;
use std::time::Duration;

use ratv_companion::message::{Event, SiriAudio, SiriStart, SiriStop};
use tokio::sync::broadcast;
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
/// to send the rest of the audio and to know that the release was sent.
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
    /// Events from the device, to see [`Event::SiriEndpoint`].
    events: broadcast::Receiver<Event>,
    /// True after [`Event::SiriEndpoint`].
    ended: bool,
}

impl Client {
    /// Holds Siri. Send the audio with [`Siri::send`] or `Siri::push_pcm`
    /// (feature `opus`), then release Siri with [`Siri::finish`].
    ///
    /// Fails with [`Error::SiriDisabled`] if Siri is off in the device settings,
    /// and with [`Error::SiriActive`] while another [`Siri`] exists.
    pub async fn start_siri(&self, options: &SiriOptions) -> Result<Siri, Error> {
        if self.siri_enabled() == Some(false) {
            return Err(Error::SiriDisabled);
        }
        if self.siri_active().swap(true, Ordering::AcqRel) {
            return Err(Error::SiriActive);
        }
        let begin = Instant::now();
        // Releases Siri and the flag if the start fails or the future is dropped.
        let siri = Siri {
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
            events: self.events(),
            ended: false,
        };
        self.request(&SiriStart {
            ptt_flags: options.ptt_flags,
        })
        .await?;
        tracing::debug!(parent: self.span(), elapsed = ?begin.elapsed(), ptt_flags = options.ptt_flags, "Siri started");
        Ok(siri)
    }
}

impl Siri {
    /// Sends one Opus packet of 20 ms, 16 kHz, mono.
    /// Send one packet every 20 ms.
    pub async fn send(&mut self, packet: &[u8]) -> Result<(), Error> {
        if self.ended() {
            return Ok(());
        }
        self.keep(packet)?;
        self.send_events(false).await
    }

    /// Sends signed 16-bit, 16 kHz, mono PCM in chunks of any size.
    /// Do not mix with [`send`](Self::send).
    #[cfg(feature = "opus")]
    pub async fn push_pcm(&mut self, pcm: &[i16]) -> Result<(), Error> {
        if self.ended() {
            return Ok(());
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
    /// Fails with [`Error::SiriNoAudio`] if no audio was given.
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
        let result = self
            .client
            .request(&SiriStop {
                ptt_flags: self.ptt_flags,
            })
            .await
            .map(drop);
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
        while !self.ended {
            match self.events.try_recv() {
                Ok(Event::SiriEndpoint) => {
                    self.ended = true;
                    tracing::debug!(parent: self.client.span(), elapsed = ?self.begin.elapsed(), packets = self.packets, "Siri endpoint");
                }
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        self.ended
    }

    fn keep(&mut self, packet: &[u8]) -> Result<(), Error> {
        keep(&mut self.data, &mut self.sizes, packet)
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
            self.next = Some(at + PACKET * count);
            let data = &self.data[self.sent_bytes..self.sent_bytes + len];
            self.client.send_event(&SiriAudio { data, sizes }).await?;
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

/// Adds a packet to the ones that are not sent yet.
fn keep(data: &mut Vec<u8>, sizes: &mut Vec<u16>, packet: &[u8]) -> Result<(), Error> {
    let size = u16::try_from(packet.len()).map_err(|_| Error::InvalidArgument("packet size"))?;
    data.extend_from_slice(packet);
    sizes.push(size);
    Ok(())
}

impl Drop for Siri {
    fn drop(&mut self) {
        if self.held {
            tracing::debug!(parent: self.client.span(), "Siri released on drop");
            let _ = self.client.request_no_wait(&SiriStop {
                ptt_flags: self.ptt_flags,
            });
        }
        self.client.siri_active().store(false, Ordering::Release);
    }
}
