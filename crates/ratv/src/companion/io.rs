use ratv_companion::{Connection, Frame, FrameType};
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

/// Sends a frame and waits for the next frame.
pub(crate) async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    conn: &mut Connection,
    ty: FrameType,
    payload: &[u8],
) -> Result<Frame, Error> {
    conn.send(ty, payload);
    flush(stream, conn).await?;
    read_frame(stream, conn).await
}

/// Runs one pairing step: wraps `tlv`, sends it and returns the TLV of the reply.
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
    let frame = exchange(stream, conn, flow.frame_type(first), buf).await?;
    Ok(ratv_companion::pairing::decode(&frame.payload)?.into_owned())
}
