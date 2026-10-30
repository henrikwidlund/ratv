//! Apple Media Remote Protocol (MRP), sans-IO.
//!
//! Every MRP message is a `ProtocolMessage` protobuf with one payload field.
//! Over AirPlay 2 the messages are sent in the data stream, each prefixed
//! with its length as a varint.
//!
//! - [`Envelope`] reads the header fields and keeps the payload encoded.
//! - [`encode`] writes a length-prefixed message.
//! - [`messages`] builds the messages a remote control sends.
//! - [`NowPlayingState`] tracks what is playing.
//! - [`artwork`] reads the image of the current item.

mod artwork;
mod envelope;
mod error;
pub mod hid;
pub mod messages;
mod player;
mod volume;

/// Protobuf types generated from the MRP `.proto` files.
#[allow(
    missing_docs,
    unreachable_pub,
    rust_2018_idioms,
    clippy::all,
    clippy::pedantic,
    rustdoc::all
)]
pub mod proto {
    include!("generated.rs");
}

pub use artwork::{Artwork, artwork};
pub use envelope::{Envelope, Frames, encode, encode_type, frames};
pub use error::Error;
pub use player::{
    Chapter, DEFAULT_PLAYER, LanguageType, MediaSubType, MediaType, NowPlaying, NowPlayingState,
    PlaybackState, QueueItem, cocoa_time, four_cc,
};
pub use volume::{OutputDevice, Volume, VolumeState, output_device_uid};

/// A message that is sent as a payload field of `ProtocolMessage`.
pub trait Payload: prost::Message + Default {
    /// Field number in `ProtocolMessage`.
    const FIELD: u32;
    /// Value of `ProtocolMessage.type`.
    const TYPE: i32;
}

macro_rules! payload {
    ($ty:ident, $field:literal, $kind:literal) => {
        impl Payload for proto::$ty {
            const FIELD: u32 = $field;
            const TYPE: i32 = $kind;
        }
    };
}

include!("payloads.rs");
