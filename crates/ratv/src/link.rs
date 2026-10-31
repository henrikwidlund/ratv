//! Connection state, shared by a client and its background task.

use std::sync::Arc;

use tokio::sync::watch;

use crate::Error;

/// Why a connection closed. `None` after `close` or when the client was dropped.
pub(crate) type Reason = Option<Arc<Error>>;

/// Set by the background task when it ends.
#[derive(Debug)]
pub(crate) struct LinkTx(watch::Sender<Option<Reason>>);

impl LinkTx {
    pub(crate) fn close(&self, reason: Reason) {
        self.0.send_replace(Some(reason));
    }
}

/// Read by the client.
#[derive(Debug, Clone)]
pub(crate) struct Link(watch::Receiver<Option<Reason>>);

impl Link {
    pub(crate) fn new() -> (LinkTx, Self) {
        let (tx, rx) = watch::channel(None);
        (LinkTx(tx), Self(rx))
    }

    pub(crate) fn is_open(&self) -> bool {
        self.0.borrow().is_none() && self.0.has_changed().is_ok()
    }

    pub(crate) async fn closed(&self) -> Reason {
        let mut rx = self.0.clone();
        rx.wait_for(Option::is_some)
            .await
            .map_or(None, |r| r.clone().flatten())
    }
}
