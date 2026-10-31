//! Logging helpers.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};

use tracing::Span;

static NEXT_SESSION: AtomicU32 = AtomicU32::new(1);

/// Span for one connection or pairing. `session` is unique in this process.
pub(crate) fn session(protocol: &'static str, addr: Option<SocketAddr>) -> Span {
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    tracing::debug_span!(
        "session",
        protocol,
        id,
        addr = addr.map(tracing::field::display)
    )
}
