//! Options for connecting and pairing, shared by all protocols.

use std::time::Duration;

use crate::Error;

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
    /// Time to wait for each response after connect. A request that is still
    /// in the outbox after this time fails with [`Error::SendTimeout`] and is
    /// not sent. A request that is still being written gives
    /// [`Error::NoAnswer`]. Also the
    /// connect limit if `connect_timeout` is `None`.
    pub timeout: Duration,
    /// Time to wait for each connect step: the TCP connect, pair-verify and
    /// each setup step. A required step that does not finish in this time
    /// fails the connect; optional ones are skipped. If `stall_timeout` is
    /// set, a stalled connect first starts again once. `None` uses `timeout`.
    /// Pairing uses [`PairOptions::timeout`].
    pub connect_timeout: Option<Duration>,
    /// If set, a connect step that gets no answer within this time cancels
    /// the connect of that protocol and starts it once more, with
    /// `connect_timeout` (or `timeout`) per step. Helps when packets are
    /// lost, for example right after the network is ready. With `None`,
    /// connect never starts again by itself. Not used unless it is below
    /// `connect_timeout` (or `timeout`). Optional Companion steps that get no
    /// answer within this time are skipped, not started again.
    pub stall_timeout: Option<Duration>,
    /// Number of requests in sequence that get no answer while the device
    /// sends nothing. Companion then closes the connection. A request that was not sent
    /// in time ([`Error::SendTimeout`]) does not count. Requests during connect
    /// and the stop request of `close` do not count either. `0` never closes it.
    /// After this close, `closed` returns [`Error::NoAnswer`].
    pub max_silent_timeouts: u32,
}

impl ClientOptions {
    /// Time to wait for each connect step.
    pub(crate) fn connect_step(&self) -> Duration {
        self.connect_timeout.unwrap_or(self.timeout)
    }

    /// Runs `connect` with `stall_timeout` per step. If a step got no answer,
    /// runs it once more with `connect_timeout` (or `timeout`) per step.
    pub(crate) async fn connect_with_retry<T, F, Fut>(&self, mut connect: F) -> Result<T, Error>
    where
        F: FnMut(Duration) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        let step = self.connect_step();
        if let Some(stall) = self.stall_timeout.filter(|s| *s < step) {
            match connect(stall).await {
                Err(err) if err.is_stall() => {
                    tracing::debug!(%err, "connect stalled, starting again");
                }
                result => return result,
            }
        }
        connect(step).await
    }
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
            connect_timeout: None,
            stall_timeout: None,
            max_silent_timeouts: 2,
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
        assert_eq!(c.stall_timeout, None);
        assert_eq!(c.connect_timeout, None);
        assert_eq!(c.connect_step(), c.timeout);
        assert_eq!(c.max_silent_timeouts, 2);
    }

    #[tokio::test]
    async fn retry_uses_connect_step() {
        let mut c = ClientOptions {
            connect_timeout: Some(Duration::from_secs(2)),
            stall_timeout: Some(Duration::from_secs(1)),
            ..ClientOptions::default()
        };
        assert_eq!(c.connect_step(), Duration::from_secs(2));
        let mut steps = Vec::new();
        let result: Result<(), Error> = c
            .connect_with_retry(|step| {
                steps.push(step);
                async { Err(Error::Timeout) }
            })
            .await;
        assert!(matches!(result, Err(Error::Timeout)));
        assert_eq!(steps, [Duration::from_secs(1), Duration::from_secs(2)]);

        // No retry when the stall limit is not below the connect limit.
        c.stall_timeout = Some(Duration::from_secs(3));
        steps.clear();
        let _ = c
            .connect_with_retry(|step| {
                steps.push(step);
                async { Err::<(), _>(Error::Timeout) }
            })
            .await;
        assert_eq!(steps, [Duration::from_secs(2)]);
    }
}
