//! AirPlay 2 control transport, sans-IO.
//!
//! A remote control session uses three TCP connections:
//!
//! 1. Control: HTTP/RTSP. Pair-verify, then encrypted [`rtsp`] requests.
//! 2. Events: the device sends RTSP requests; we answer each with 200.
//! 3. Data stream: MRP messages in [`datastream`] frames.
//!
//! All three use [`FrameCipher`] after pair-verify, with keys from [`keys`].

mod channel;
pub mod datastream;
mod error;
mod frame;
pub mod http;
pub mod keys;
pub mod rtsp;

pub use channel::Channel;
pub use error::Error;
pub use frame::{BLOCK_LEN, FrameCipher};
