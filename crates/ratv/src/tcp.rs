//! TCP connections.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::Error;

/// Connects with a timeout and turns off Nagle's algorithm.
pub(crate) async fn connect(addr: SocketAddr, timeout: Duration) -> Result<TcpStream, Error> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect(addr)).await??;
    stream.set_nodelay(true)?;
    Ok(stream)
}
