//! Apple TV Companion protocol (also called Rapport), sans-IO.
//!
//! The Companion protocol runs over TCP (service `_companion-link._tcp`).
//! Frames have a 4-byte header and carry OPACK payloads. After pair-verify,
//! frames are encrypted with ChaCha20-Poly1305.
//!
//! - [`Connection`]: framing and encryption. Takes received bytes, returns bytes to send.
//! - [`message`]: request/event encoding and response decoding.
//! - [`pairing`]: wraps HAP pairing TLV8 in Companion frames.
//! - [`text_input`]: keyboard text.

mod connection;
mod error;
mod frame;
pub mod message;
pub mod pairing;
pub mod text_input;

pub use connection::{Connection, DEFAULT_MAX_FRAME_LEN};
pub use error::Error;
pub use frame::{Frame, FrameType, HEADER_LEN};
