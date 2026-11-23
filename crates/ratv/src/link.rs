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

/// Place of a queued message in an outbox buffer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Queued {
    batch: u64,
    /// Range as if no message was removed from the buffer.
    start: usize,
    end: usize,
}

/// Tracks when the background task takes the outbox buffer, so that a
/// message it has not taken can still be removed.
#[derive(Debug, Default)]
pub(crate) struct Batches {
    /// Number of times the task took the buffer.
    taken: u64,
    /// Bytes removed from the current buffer.
    removed_len: usize,
    /// Start (as in [`Queued`]) and length of each removed message.
    removed: Vec<(usize, usize)>,
}

impl Batches {
    /// Place of the bytes from `start` to `end` in the current buffer.
    pub(crate) const fn queued(&self, start: usize, end: usize) -> Queued {
        Queued {
            batch: self.taken,
            start: start + self.removed_len,
            end: end + self.removed_len,
        }
    }

    /// Call when the task takes the buffer.
    pub(crate) fn take(&mut self) {
        self.taken = self.taken.wrapping_add(1);
        self.removed_len = 0;
        self.removed.clear();
    }

    /// Moves the bytes after the message over it and returns the length
    /// without it, if the task has not taken it. Truncate `buf` to that length.
    pub(crate) fn remove(&mut self, q: Queued, buf: &mut [u8]) -> Option<usize> {
        if q.batch != self.taken {
            return None;
        }
        debug_assert!(
            !self.removed.iter().any(|(start, _)| *start == q.start),
            "removed twice"
        );
        let shift: usize = self
            .removed
            .iter()
            .filter(|(start, _)| *start < q.start)
            .map(|(_, len)| len)
            .sum();
        let (start, len) = (q.start - shift, q.end - q.start);
        buf.copy_within(start + len.., start);
        self.removed_len += len;
        self.removed.push((q.start, len));
        Some(buf.len() - len)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn batches_remove_untaken_messages() {
        let mut batches = Batches::default();
        let mut buf = b"aabbbcc".to_vec();
        let first = batches.queued(0, 2);
        let middle = batches.queued(2, 5);
        let last = batches.queued(5, 7);
        let remove = |batches: &mut Batches, q, buf: &mut Vec<u8>| {
            let len = batches.remove(q, buf);
            buf.truncate(len.unwrap_or(buf.len()));
            len.is_some()
        };
        assert!(remove(&mut batches, middle, &mut buf));
        assert_eq!(buf, b"aacc");
        // Pushed after a removal: its range is in the current buffer.
        buf.extend_from_slice(b"dd");
        let added = batches.queued(4, 6);
        assert!(remove(&mut batches, first, &mut buf));
        assert_eq!(buf, b"ccdd");
        assert!(remove(&mut batches, added, &mut buf));
        assert_eq!(buf, b"cc");
        assert!(remove(&mut batches, last, &mut buf));
        assert_eq!(buf, b"");

        buf.extend_from_slice(b"ee");
        let taken = batches.queued(0, 2);
        batches.take();
        assert!(!remove(&mut batches, taken, &mut buf));
        assert_eq!(buf, b"ee");
    }

    /// Random push, remove and take steps, checked against a list of messages.
    #[test]
    fn batches_match_model() {
        // xorshift64 with a fixed seed, so a failure can be repeated.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            usize::try_from(state % n as u64).unwrap()
        };
        let mut batches = Batches::default();
        let mut buf = Vec::new();
        // Messages in `buf`, in order, with their places.
        let mut model: Vec<(Queued, Vec<u8>)> = Vec::new();
        // Places from batches that were taken. Each is removed at most once.
        let mut taken: Vec<Queued> = Vec::new();
        let mut byte = 0u8;
        for _ in 0..20_000 {
            match next(10) {
                0..=4 => {
                    let msg: Vec<u8> = (0..=next(6))
                        .map(|_| {
                            byte = byte.wrapping_add(1);
                            byte
                        })
                        .collect();
                    let start = buf.len();
                    buf.extend_from_slice(&msg);
                    model.push((batches.queued(start, buf.len()), msg));
                }
                5..=7 if !model.is_empty() => {
                    let (q, _) = model.remove(next(model.len()));
                    let len = batches.remove(q, &mut buf).unwrap();
                    buf.truncate(len);
                }
                8 if !taken.is_empty() => {
                    let q = taken.swap_remove(next(taken.len()));
                    assert!(batches.remove(q, &mut buf).is_none());
                }
                9 => {
                    batches.take();
                    buf.clear();
                    taken.extend(std::mem::take(&mut model).into_iter().map(|(q, _)| q));
                }
                _ => {}
            }
            let expected: Vec<u8> = model.iter().flat_map(|(_, m)| m).copied().collect();
            assert_eq!(buf, expected);
        }
    }

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
