//! TCP connections.

use std::net::SocketAddr;
use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::Error;

/// Idle time before the first keepalive probe.
const KEEPALIVE_TIME: Duration = Duration::from_secs(10);
/// Time that sent data can stay without acknowledgement before the connection
/// fails. Keepalive does not run while sent data waits for acknowledgement.
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
/// On most systems, keepalive finds a device that is gone without closing the
/// connection, for example after a power loss, in about 25 s. While sent data
/// waits for an acknowledgement, keepalive may not run. On Linux and Android,
/// a send time limit then finds the device in about 25 s. Options that the
/// system refuses are skipped.
pub(crate) async fn connect(addr: SocketAddr, timeout: Duration) -> Result<TcpStream, Error> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect(addr)).await??;
    let _ = stream.set_nodelay(true);
    let socket = SockRef::from(&stream);
    let _ = socket.set_tcp_keepalive(&with_probes(TcpKeepalive::new().with_time(KEEPALIVE_TIME)));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = socket.set_tcp_user_timeout(Some(USER_TIMEOUT));
    Ok(stream)
}

/// Sends a probe every 5 s after the idle time, and fails after 3 probes
/// without an answer.
#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "illumos",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "windows",
))]
const fn with_probes(keepalive: TcpKeepalive) -> TcpKeepalive {
    keepalive
        .with_interval(Duration::from_secs(5))
        .with_retries(3)
}

/// Keeps the probe values of the system.
#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "illumos",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "windows",
)))]
const fn with_probes(keepalive: TcpKeepalive) -> TcpKeepalive {
    keepalive
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
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        {
            assert_eq!(socket.tcp_keepalive_time().unwrap(), KEEPALIVE_TIME);
            assert_eq!(
                socket.tcp_keepalive_interval().unwrap(),
                Duration::from_secs(5)
            );
            assert_eq!(socket.tcp_keepalive_retries().unwrap(), 3);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(socket.tcp_user_timeout().unwrap(), Some(USER_TIMEOUT));
    }
}
