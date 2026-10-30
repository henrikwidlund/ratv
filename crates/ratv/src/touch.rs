//! Swipe timing for Companion touch events.

use std::time::Duration;

use crate::Error;

/// Time between touch events of a swipe.
pub(crate) const STEP: Duration = Duration::from_millis(16);

/// Phase of a swipe event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Begin,
    Move,
    End,
}

/// Positions between `from` and `to`, one per [`STEP`], without the ends.
fn path(from: (f64, f64), to: (f64, f64), duration: Duration) -> impl Iterator<Item = (f64, f64)> {
    let steps = u32::try_from(duration.as_millis() / STEP.as_millis())
        .unwrap_or(u32::MAX)
        .max(1);
    (1..steps).map(move |i| {
        let t = f64::from(i) / f64::from(steps);
        (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
    })
}

/// Sends a swipe from `from` to `to` that takes about `duration`.
///
/// Waits one step before the first move, so the device sees the press first.
pub(crate) async fn swipe<F, Fut>(
    from: (f64, f64),
    to: (f64, f64),
    duration: Duration,
    mut send: F,
) -> Result<(), Error>
where
    F: FnMut(f64, f64, Phase) -> Fut,
    Fut: Future<Output = Result<(), Error>>,
{
    send(from.0, from.1, Phase::Begin).await?;
    for (x, y) in path(from, to, duration) {
        tokio::time::sleep(STEP).await;
        send(x, y, Phase::Move).await?;
    }
    tokio::time::sleep(STEP).await;
    send(to.0, to.1, Phase::End).await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn path_stays_between_ends() {
        let points: Vec<_> = path((0.8, 0.5), (0.2, 0.5), Duration::from_millis(160)).collect();
        assert_eq!(points.len(), 9);
        assert!(
            points
                .iter()
                .all(|p| (0.2..=0.8).contains(&p.0) && (p.1 - 0.5).abs() < 1e-9)
        );
        assert!(points.windows(2).all(|w| w[1].0 < w[0].0));
        assert_eq!(path((0.0, 0.0), (1.0, 1.0), Duration::ZERO).count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn swipe_events() {
        let events = Mutex::new(Vec::new());
        swipe(
            (0.0, 0.0),
            (1.0, 0.0),
            Duration::from_millis(48),
            |x, _, p| {
                events
                    .lock()
                    .unwrap()
                    .push((x, p, tokio::time::Instant::now()));
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
        let events = events.into_inner().unwrap();
        let phases: Vec<_> = events.iter().map(|e| e.1).collect();
        assert_eq!(phases, [Phase::Begin, Phase::Move, Phase::Move, Phase::End]);
        assert!((events[3].0 - 1.0).abs() < 1e-9);
        // One step between all events, also before the first move.
        assert!(events.windows(2).all(|w| w[1].2 - w[0].2 == STEP));
    }
}
