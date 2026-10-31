//! TCP connections.

use std::net::SocketAddr;
use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::Error;

/// Idle time before the first keepalive probe.
const KEEPALIVE_TIME: Duration = Duration::from_secs(10);
/// Time between keepalive probes.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Probes without answer before the connection fails.
const KEEPALIVE_RETRIES: u32 = 3;
/// Time that sent data can stay without acknowledgement before the connection fails.
#[cfg(any(target_os = "linux", target_os = "android"))]
const USER_TIMEOUT: Duration = Duration::from_secs(25);

/// Time a write can wait for the device to read.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Writes and flushes all of `buf`. Fails with [`Error::Timeout`] if the
/// device does not read it in time.
pub(crate) async fn write_all<W: AsyncWrite + Unpin>(w: &mut W, buf: &[u8]) -> Result<(), Error> {
    tokio::time::timeout(WRITE_TIMEOUT, async {
        w.write_all(buf).await?;
        w.flush().await
    })
    .await??;
    Ok(())
}

/// Connects with a timeout, turns off Nagle's algorithm and turns on keepalive.
///
/// Keepalive finds a device that is gone without closing the connection,
/// for example after a power loss, in about 25 s.
pub(crate) async fn connect(addr: SocketAddr, timeout: Duration) -> Result<TcpStream, Error> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect(addr)).await??;
    stream.set_nodelay(true)?;
    let socket = SockRef::from(&stream);
    socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(KEEPALIVE_TIME)
            .with_interval(KEEPALIVE_INTERVAL)
            .with_retries(KEEPALIVE_RETRIES),
    )?;
    // Keepalive does not run while sent data waits for acknowledgement.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    socket.set_tcp_user_timeout(Some(USER_TIMEOUT))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sets_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = connect(addr, Duration::from_secs(1)).await.unwrap();
        let socket = SockRef::from(&stream);
        assert!(socket.keepalive().unwrap());
        assert!(stream.nodelay().unwrap());
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(socket.tcp_user_timeout().unwrap(), Some(USER_TIMEOUT));
    }
}
