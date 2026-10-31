//! Options for connecting and pairing, shared by all protocols.

use std::time::Duration;

/// Options for connecting to a device.
///
/// Start from `ClientOptions::default()` and change the fields you need.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientOptions {
    /// Name we report to the device.
    pub name: String,
    /// Model we report, for example `iPhone14,3`.
    pub model: String,
    /// iOS version we report. Used by MRP.
    pub os_version: String,
    /// iOS build we report. Used by MRP.
    pub os_build: String,
    /// Device id, `AA:BB:CC:DD:EE:FF`. If `None`, it is made from the
    /// credentials, so it is the same on each connect.
    pub device_id: Option<String>,
    /// Unique id for `DEVICE_INFO`, usually a UUID. If `None`, it is made
    /// from the credentials, so it is the same on each connect. Used by MRP.
    pub unique_identifier: Option<String>,
    /// Time to wait for the connection and each response.
    pub timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            name: "ratv".into(),
            model: "iPhone14,3".into(),
            os_version: "17.0".into(),
            os_build: "21A329".into(),
            device_id: None,
            unique_identifier: None,
            timeout: Duration::from_secs(5),
        }
    }
}

/// Options for pairing.
///
/// Start from `PairOptions::default()` and change the fields you need.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PairOptions {
    /// Name shown with the PIN and in the device settings.
    pub name: String,
    /// Model we report, for example `iPhone14,3`. Used by Companion.
    pub model: String,
    /// Pairing identifier. A random UUID is used when `None`.
    pub client_id: Option<String>,
    /// Time to wait for the connection and each response.
    pub timeout: Duration,
}

impl Default for PairOptions {
    fn default() -> Self {
        Self {
            name: "ratv".into(),
            model: "iPhone14,3".into(),
            client_id: None,
            timeout: Duration::from_secs(5),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = ClientOptions::default();
        let p = PairOptions::default();
        assert_eq!(
            (c.timeout, p.timeout),
            (Duration::from_secs(5), Duration::from_secs(5))
        );
        assert_eq!(c.model, p.model);
    }
}
