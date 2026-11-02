use std::time::Duration;

use ratv_companion::pairing::Flow;
use ratv_companion::{Connection, Frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use crate::Error;

const READ_CHUNK: usize = 4096;

/// Writes all queued bytes.
pub(crate) async fn flush<S: AsyncWrite + Unpin>(
    stream: &mut S,
    conn: &mut Connection,
) -> Result<(), Error> {
    if conn.has_transmit() {
        crate::tcp::write_all(stream, &conn.take_transmit()).await?;
    }
    Ok(())
}

/// Reads until one complete frame is available.
pub(crate) async fn read_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    conn: &mut Connection,
) -> Result<Frame, Error> {
    loop {
        if let Some(frame) = conn.poll_frame()? {
            return Ok(frame);
        }
        if stream.read_buf(conn.receive_buffer(READ_CHUNK)).await? == 0 {
            return Err(Error::Disconnected);
        }
    }
}

/// Most skipped frames kept in [`Error::PairingStep`].
const MAX_SKIPPED: usize = 8;

/// A pairing message we send. The reply is the next message.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Step {
    SetupM1,
    SetupM3,
    SetupM5,
    VerifyM1,
    VerifyM3,
}

impl Step {
    const fn flow(self) -> Flow {
        match self {
            Self::SetupM1 | Self::SetupM3 | Self::SetupM5 => Flow::Setup,
            Self::VerifyM1 | Self::VerifyM3 => Flow::Verify,
        }
    }

    const fn first(self) -> bool {
        matches!(self, Self::SetupM1 | Self::VerifyM1)
    }

    /// The reply we wait for.
    const fn reply(self) -> &'static str {
        match self {
            Self::SetupM1 => "pair-setup M2",
            Self::SetupM3 => "pair-setup M4",
            Self::SetupM5 => "pair-setup M6",
            Self::VerifyM1 => "pair-verify M2",
            Self::VerifyM3 => "pair-verify M4",
        }
    }
}

/// Runs one pairing step: wraps `tlv`, sends it and returns the TLV of the reply.
/// Frames of other types before the reply are skipped. All errors are
/// returned as [`Error::PairingStep`].
pub(crate) async fn pairing_step<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    conn: &mut Connection,
    step: Step,
    tlv: &[u8],
    buf: &mut Vec<u8>,
    timeout: Duration,
) -> Result<Vec<u8>, Error> {
    let flow = step.flow();
    buf.clear();
    ratv_companion::pairing::encode(flow, tlv, buf);
    conn.send(flow.frame_type(step.first()), buf);
    let reply = flow.frame_type(false);
    let mut skipped = Vec::new();
    let result = tokio::time::timeout(timeout, async {
        flush(stream, conn).await?;
        loop {
            let frame = read_frame(stream, conn).await?;
            if frame.ty == reply {
                return Ok(ratv_companion::pairing::decode_reply(&frame)?.into_owned());
            }
            tracing::warn!(
                ty = frame.ty.0,
                len = frame.payload.len(),
                "skipped frame while waiting for pairing reply"
            );
            if skipped.len() < MAX_SKIPPED {
                skipped.push((frame.ty.0, frame.payload.len()));
            }
        }
    })
    .await;
    result
        .unwrap_or(Err(Error::Timeout))
        .map_err(|e| Error::PairingStep {
            step: step.reply(),
            skipped,
            source: Box::new(e),
        })
}
