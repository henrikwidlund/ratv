//! Control Apple TV devices on the local network.
//!
//! - [`AppleTv`]: one handle that uses Companion and MRP together.
//! - [`companion`]: pairing, buttons, apps, power and volume over the Companion protocol.
//! - [`mrp`]: now playing, playback commands and buttons over AirPlay 2.
//! - [`discovery`]: find devices with mDNS.
//!
//! The protocol crates (`ratv-codec`, `ratv-hap`, `ratv-companion`,
//! `ratv-airplay`, `ratv-mrp`) are sans-IO. This crate runs them with tokio.
//!
//! ```no_run
//! # async fn run() -> Result<(), ratv::Error> {
//! use ratv::companion::{self, ClientOptions, HidCommand};
//!
//! let addr = "192.168.1.20:49153".parse().unwrap();
//!
//! // First time: pair with the PIN shown on the TV and store the credentials.
//! let prompt = companion::pair(addr, &companion::PairOptions::default()).await?;
//! let credentials = prompt.finish("1234").await?;
//! let stored = credentials.to_secret_string();
//!
//! // Later: connect with the stored credentials.
//! use ratv::ExposeSecret;
//! let credentials = stored.expose_secret().parse().unwrap();
//! let client = companion::Client::connect(addr, &credentials, &ClientOptions::default()).await?;
//! client.press(HidCommand::Select).await?;
//! client.set_volume(0.3).await?;
//! client.launch_app("com.netflix.Netflix").await?;
//! # Ok(())
//! # }
//! ```

#[cfg(feature = "mrp")]
mod airplay;
#[cfg(all(feature = "companion", feature = "mrp"))]
mod apple_tv;
#[cfg(feature = "companion")]
pub mod companion;
#[cfg(feature = "discovery")]
pub mod discovery;
mod error;
#[cfg(any(feature = "companion", feature = "mrp"))]
mod log;
#[cfg(feature = "mrp")]
pub mod mrp;
#[cfg(any(feature = "companion", feature = "mrp"))]
mod rng;
#[cfg(any(feature = "companion", feature = "mrp"))]
mod tcp;
#[cfg(feature = "companion")]
mod touch;

#[cfg(all(feature = "companion", feature = "mrp"))]
pub use apple_tv::{AppleTv, ConnectOptions, Endpoints, Key};
pub use error::Error;

#[cfg(all(doctest, feature = "companion", feature = "mrp", feature = "discovery"))]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;
pub use ratv_hap::{Credentials, ExposeSecret, SecretString};
