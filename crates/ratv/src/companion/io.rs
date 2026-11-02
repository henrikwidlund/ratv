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

/// Runs one pairing step: wraps `tlv`, sends it and returns the TLV of the reply.
/// Frames of other types before the reply are skipped.
pub(crate) async fn pairing_step<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    conn: &mut Connection,
    flow: ratv_companion::pairing::Flow,
    first: bool,
    tlv: &[u8],
    buf: &mut Vec<u8>,
) -> Result<Vec<u8>, Error> {
    buf.clear();
    ratv_companion::pairing::encode(flow, tlv, buf);
    conn.send(flow.frame_type(first), buf);
    flush(stream, conn).await?;
    let reply = flow.frame_type(false);
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
    }
}
