//! Wire formats used by Apple TV protocols.
//!
//! - [`bplist`]: binary property lists used by AirPlay.
//! - [`opack`]: Apple's compact binary serialization used by the Companion protocol.
//! - [`tlv8`]: HomeKit TLV8 used during pairing.
//!
//! Decoders borrow from the input buffer where possible and never allocate for
//! values that fit in a single fragment.

pub mod bplist;
pub mod opack;
pub mod tlv8;
