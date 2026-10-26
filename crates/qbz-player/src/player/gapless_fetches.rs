//! Which gapless successor is being downloaded, so a load of that very track
//! can wait for it instead of downloading it a second time.
//!
//! A skip to the next track usually lands while that track is being fetched
//! for gapless: the fetch starts five seconds into every track. The load then
//! opened a stream of its own beside the fetch — the same file down twice — and
//! on the Pi that stream's first range took 5 s to answer, while the fetch
//! finished in that time and was thrown away (2026-10-27, track 446220448).
//! The engine now races its stream against the fetch and plays from the cache
//! when the fetch wins: `DaemonRendererEngine::start_track_stream` in pibuz.

use std::future::Future;
use std::sync::Mutex;

use tokio::sync::watch;

/// The gapless fetch in flight, if any. One at a time: the driver never runs two.
#[derive(Default)]
pub(crate) struct GaplessFetches {
    in_flight: Mutex<Option<(u64, watch::Receiver<()>)>>,
}

/// Holds a fetch's place in [`GaplessFetches`]. Dropping it — the fetch
/// returning, whatever it returned, or its future being dropped — wakes every
/// [`GaplessFetches::finished`] waiting on it.
pub(crate) struct InFlight<'a> {
    fetches: &'a GaplessFetches,
    track_id: u64,
    // Never sent on: dropping it is the signal, so no path out of the fetch
    // can forget to give it.
    _done: watch::Sender<()>,
}

impl GaplessFetches {
    /// Record `track_id` as being fetched until the returned guard drops.
    pub(crate) fn begin(&self, track_id: u64) -> InFlight<'_> {
        let (done, waiters) = watch::channel(());
        if let Ok(mut in_flight) = self.in_flight.lock() {
            *in_flight = Some((track_id, waiters));
        }
        InFlight {
            fetches: self,
            track_id,
            _done: done,
        }
    }

    /// While `track_id` is being fetched: a future that completes once that
    /// fetch is over, however it ended. By then a fetch that succeeded has put
    /// the track in the L1 or L2 cache.
    pub(crate) fn finished(
        &self,
        track_id: u64,
    ) -> Option<impl Future<Output = ()> + Send + 'static> {
        let mut waiter = self
            .in_flight
            .lock()
            .ok()?
            .as_ref()
            .filter(|(id, _)| *id == track_id)?
            .1
            .clone();
        // Nothing is ever sent, so this returns when the sender drops.
        Some(async move {
            let _ = waiter.changed().await;
        })
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        // Cleared before `_done` drops, so a woken waiter never finds the fetch
        // still listed.
        if let Ok(mut in_flight) = self.fetches.in_flight.lock() {
            if in_flight.as_ref().map(|(id, _)| *id) == Some(self.track_id) {
                *in_flight = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn only_the_track_being_fetched_can_be_waited_for() {
        let fetches = GaplessFetches::default();
        assert!(fetches.finished(7).is_none(), "nothing in flight");
        let _in_flight = fetches.begin(7);
        assert!(fetches.finished(8).is_none(), "another track");
        assert!(fetches.finished(7).is_some());
    }

    /// A waiter wakes when the fetch ends, and not before.
    #[test]
    fn a_waiter_wakes_when_the_fetch_ends() {
        let fetches = GaplessFetches::default();
        let in_flight = fetches.begin(7);
        let waiting = fetches.finished(7).expect("in flight");
        let mut waiting = std::pin::pin!(waiting);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(waiting.as_mut().poll(&mut cx).is_pending());

        drop(in_flight);
        assert!(waiting.as_mut().poll(&mut cx).is_ready());
        assert!(
            fetches.finished(7).is_none(),
            "an ended fetch is not listed"
        );
    }

    /// The driver drops a called-off fetch's future at shutdown, and a load
    /// waiting on it must not wait for ever.
    #[tokio::test]
    async fn a_dropped_fetch_wakes_its_waiters_too() {
        let fetches = GaplessFetches::default();
        let waiting = {
            let in_flight = fetches.begin(7);
            let waiting = fetches.finished(7).expect("in flight");
            let fetch = async move {
                let _in_flight = in_flight;
                std::future::pending::<()>().await;
            };
            drop(fetch);
            waiting
        };
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the waiter woke");
    }

    /// A guard from an earlier fetch going late does not unlist the next one.
    #[test]
    fn an_earlier_fetch_ending_late_leaves_the_next_one_listed() {
        let fetches = GaplessFetches::default();
        let first = fetches.begin(7);
        let _second = fetches.begin(8);
        drop(first);
        assert!(fetches.finished(8).is_some());
    }
}
