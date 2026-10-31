//! Encodes 16 kHz mono PCM into the Opus packets that voice input needs.

use rusty_opus::{Application, OpusEncoder, SignalType};

use crate::Error;
use crate::messages::{VOICE_FRAMES, VOICE_SAMPLE_RATE};

/// Samples in one frame.
const FRAME: usize = VOICE_FRAMES as usize;
/// Largest Opus packet.
const MAX_PACKET: usize = 1275;
/// Bitrate for speech at 16 kHz.
const BITRATE: i32 = 24_000;

/// Cuts PCM into 20 ms frames and encodes each frame as one Opus packet.
///
/// Input is signed 16-bit, 16 kHz, mono, in chunks of any size.
pub struct VoiceEncoder {
    encoder: OpusEncoder,
    /// Samples of the next frame.
    frame: [f32; FRAME],
    len: usize,
    packet: [u8; MAX_PACKET],
}

impl VoiceEncoder {
    /// Creates an encoder for speech.
    #[must_use]
    pub fn new() -> Self {
        #[allow(clippy::cast_possible_wrap)]
        let mut encoder = OpusEncoder::new(VOICE_SAMPLE_RATE as i32, 1, Application::Voip)
            .expect("16 kHz mono is a valid Opus configuration");
        encoder.bitrate_bps = BITRATE;
        encoder.signal_type = Some(SignalType::Voice);
        encoder.lsb_depth = 16;
        Self {
            encoder,
            frame: [0.0; FRAME],
            len: 0,
            packet: [0; MAX_PACKET],
        }
    }

    /// Adds samples. Calls `packet` with each complete frame.
    pub fn push(&mut self, mut pcm: &[i16], mut packet: impl FnMut(&[u8])) -> Result<(), Error> {
        while !pcm.is_empty() {
            let n = pcm.len().min(FRAME - self.len);
            for (dst, &src) in self.frame[self.len..self.len + n].iter_mut().zip(&pcm[..n]) {
                *dst = f32::from(src) / 32_768.0;
            }
            self.len += n;
            pcm = &pcm[n..];
            if self.len == FRAME {
                packet(self.encode()?);
            }
        }
        Ok(())
    }

    /// Fills the last frame with silence and encodes it. Does nothing if no samples are left.
    pub fn flush(&mut self, mut packet: impl FnMut(&[u8])) -> Result<(), Error> {
        if self.len > 0 {
            self.frame[self.len..].fill(0.0);
            packet(self.encode()?);
        }
        Ok(())
    }

    fn encode(&mut self) -> Result<&[u8], Error> {
        self.len = 0;
        let n = self
            .encoder
            .encode(&self.frame, FRAME, &mut self.packet)
            .map_err(|e| match e {
                rusty_opus::Error::BadArg(m)
                | rusty_opus::Error::BufferTooSmall(m)
                | rusty_opus::Error::InvalidPacket(m)
                | rusty_opus::Error::Internal(m) => Error::Encode(m),
                _ => Error::Encode("unknown error"),
            })?;
        Ok(&self.packet[..n])
    }
}

impl Default for VoiceEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for VoiceEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceEncoder")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use rusty_opus::OpusDecoder;

    use super::*;

    /// 440 Hz tone.
    fn tone(samples: u16) -> Vec<i16> {
        (0..samples)
            .map(|i| {
                let t = f32::from(i) / 16_000.0;
                (f32::sin(t * 440.0 * std::f32::consts::TAU) * 8_000.0) as i16
            })
            .collect()
    }

    #[test]
    fn frames_across_chunks() {
        let mut enc = VoiceEncoder::new();
        let mut packets = Vec::new();
        let pcm = tone(1_000);
        // 100 + 600 samples: two full frames, 60 samples left.
        enc.push(&pcm[..100], |p| packets.push(p.to_vec())).unwrap();
        enc.push(&pcm[100..700], |p| packets.push(p.to_vec()))
            .unwrap();
        assert_eq!(packets.len(), 2);
        enc.flush(|p| packets.push(p.to_vec())).unwrap();
        assert_eq!(packets.len(), 3);
        // Nothing left after a flush.
        enc.flush(|p| packets.push(p.to_vec())).unwrap();
        assert_eq!(packets.len(), 3);

        // Each packet decodes to 20 ms at 16 kHz.
        let mut dec = OpusDecoder::new(16_000, 1).unwrap();
        let mut out = [0.0f32; FRAME];
        for p in &packets {
            assert_eq!(dec.decode(p, FRAME, &mut out).unwrap(), FRAME);
        }
        assert!(out.iter().any(|&s| s != 0.0));
    }
}
