//! Wire formats used by Apple TV protocols.
//!
//! - [`bplist`]: binary property lists used by AirPlay.
//! - [`opack`]: Apple's compact binary serialization used by the Companion protocol.
//! - [`tlv8`]: HomeKit TLV8 used during pairing.
//!
//! Decoders borrow strings and data from the input buffer. They allocate for
//! containers, for UTF-16 strings in plists, for the TLV8 item list and joined
//! TLV8 fragments, and for an OPACK object table of more than 16 values.

pub mod bplist;
pub mod opack;
pub mod tlv8;
