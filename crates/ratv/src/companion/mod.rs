//! Companion protocol client: remote buttons, media control, volume, apps, power, text input and Siri.
//!
//! Connect to the port of the `_companion-link._tcp` service.

mod client;
mod io;
mod pairing;
mod siri;

pub use crate::{ClientOptions, PairOptions};
pub use client::Client;
pub use pairing::{PinPrompt, pair, pair_stream};
pub use ratv_companion::message::{
    Account, App, AttentionState, ButtonState, Event, HidCommand, MediaControl, MediaControlFlags,
    TouchPhase,
};
pub use siri::{Siri, SiriOptions};
