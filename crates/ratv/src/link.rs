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

/// Closes the link and runs `cleanup` when dropped. This also happens when
/// the background task panics or is aborted.
pub(crate) struct CloseGuard<F: FnMut()> {
    link: LinkTx,
    /// Reason given to the link. `None` until the task sets it.
    pub(crate) reason: Reason,
    cleanup: F,
}

impl<F: FnMut()> CloseGuard<F> {
    pub(crate) const fn new(link: LinkTx, cleanup: F) -> Self {
        Self {
            link,
            reason: None,
            cleanup,
        }
    }
}

impl<F: FnMut()> Drop for CloseGuard<F> {
    fn drop(&mut self) {
        self.link.close(self.reason.take());
        (self.cleanup)();
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[tokio::test]
    async fn guard_closes_with_reason() {
        let (tx, link) = Link::new();
        let cleaned = Cell::new(false);
        let mut guard = CloseGuard::new(tx, || cleaned.set(true));
        assert!(link.is_open());
        guard.reason = Some(Arc::new(Error::Disconnected));
        drop(guard);
        assert!(cleaned.get());
        assert!(!link.is_open());
        assert!(matches!(
            link.closed().await.as_deref(),
            Some(Error::Disconnected)
        ));
    }

    #[test]
    fn guard_cleans_up_on_panic() {
        let (tx, link) = Link::new();
        let cleaned = Cell::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = CloseGuard::new(tx, || cleaned.set(true));
            panic!("task failed");
        }));
        assert!(result.is_err());
        assert!(cleaned.get());
        assert!(!link.is_open());
    }
}
