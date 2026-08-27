use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tokio::time::{Instant, sleep_until};

pub(crate) const EVENT_TRIGGER_LIMIT: usize = 25;
pub(crate) const EVENT_DEBOUNCE: Duration = Duration::from_millis(500);
pub(crate) const SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(300);
pub(crate) const PUSHER_OUTAGE_POLL_INTERVAL: Duration = Duration::from_secs(6);

#[derive(Clone, Debug)]
pub(crate) struct PusherStatus {
    connected: watch::Sender<bool>,
}

impl PusherStatus {
    pub(crate) fn new() -> Self {
        let (connected, _) = watch::channel(false);
        Self { connected }
    }

    pub(crate) fn set_connected(&self, connected: bool) {
        self.connected.send_if_modified(|current| {
            if *current == connected {
                false
            } else {
                *current = connected;
                true
            }
        });
    }

    pub(crate) fn poller(&self) -> PusherAwarePoller {
        PusherAwarePoller::new(self.connected.subscribe())
    }
}

impl Default for PusherStatus {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct PusherAwarePoller {
    connected: watch::Receiver<bool>,
    deadline: Instant,
}

impl PusherAwarePoller {
    fn new(connected: watch::Receiver<bool>) -> Self {
        let deadline = Instant::now() + poll_interval(*connected.borrow());
        Self {
            connected,
            deadline,
        }
    }

    /// Wait for the next fallback/safety poll. A Pusher state transition
    /// restarts the timer using the interval appropriate to the new state.
    pub(crate) async fn tick(&mut self) {
        loop {
            if self.connected.has_changed().unwrap_or(false) {
                let connected = *self.connected.borrow_and_update();
                self.deadline = Instant::now() + poll_interval(connected);
            }

            tokio::select! {
                _ = sleep_until(self.deadline) => {
                    self.deadline = Instant::now() + poll_interval(*self.connected.borrow());
                    return;
                }
                changed = self.connected.changed() => {
                    let connected = changed
                        .map(|()| *self.connected.borrow_and_update())
                        .unwrap_or(false);
                    self.deadline = Instant::now() + poll_interval(connected);
                }
            }
        }
    }
}

fn poll_interval(pusher_connected: bool) -> Duration {
    if pusher_connected {
        SAFETY_POLL_INTERVAL
    } else {
        PUSHER_OUTAGE_POLL_INTERVAL
    }
}

#[derive(Debug, Default)]
struct TriggerState {
    /// Event identifiers received since the current fresh lookup began.
    pending_ids: HashSet<String>,
    /// Identifiers in the batch currently being processed. Events for these
    /// identifiers are duplicates and must not schedule a follow-up lookup.
    active_ids: HashSet<String>,
    /// Startup, reconnect, or another non-event reason to perform a lookup.
    forced: bool,
    /// Trailing-edge debounce deadline for `pending_ids` while idle.
    debounce_deadline: Option<Instant>,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<TriggerState>,
    notify: Notify,
}

/// Shared event state for one independently-serialised worker.
///
/// Recording an event is synchronous and fast, so the WebSocket reader never
/// waits for a worker that is busy signing or submitting a mutation.
#[derive(Clone, Debug)]
pub(crate) struct WorkTrigger {
    inner: Arc<Inner>,
}

impl WorkTrigger {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(TriggerState::default()),
                notify: Notify::new(),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, TriggerState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record one unique platform event. Events matching the active batch are
    /// already prepared and therefore do not create deferred work.
    pub(crate) fn record_event(&self, id: String) {
        let inserted = {
            let mut state = self.state();
            if state.active_ids.contains(&id) {
                false
            } else {
                let inserted = state.pending_ids.insert(id);
                if inserted {
                    state.debounce_deadline = Some(Instant::now() + EVENT_DEBOUNCE);
                }
                inserted
            }
        };

        if inserted {
            self.inner.notify.notify_one();
        }
    }

    /// Force a lookup for startup or after a successful WebSocket reconnect.
    pub(crate) fn force(&self) {
        self.state().forced = true;
        self.inner.notify.notify_one();
    }

    /// Wait for a force, 25 unique events, or the trailing debounce deadline.
    pub(crate) async fn wait_until_ready(&self) {
        loop {
            // Register before checking state so a notification racing with the
            // check is retained by `Notify` instead of being lost.
            let notified = self.inner.notify.notified();
            let deadline = {
                let state = self.state();
                if state.forced
                    || state.pending_ids.len() >= EVENT_TRIGGER_LIMIT
                    || state
                        .debounce_deadline
                        .is_some_and(|deadline| deadline <= Instant::now())
                {
                    return;
                }
                state.debounce_deadline
            };

            match deadline {
                Some(deadline) => {
                    tokio::select! {
                        _ = notified => {}
                        _ = sleep_until(deadline) => {}
                    }
                }
                None => notified.await,
            }
        }
    }

    /// Start a lookup from `cursor: None`. Events already pending are covered
    /// by this authoritative scan. Events arriving after this call remain in
    /// `pending_ids` and can force a post-processing scan.
    pub(crate) fn begin_fresh_lookup(&self) {
        let mut state = self.state();
        state.pending_ids.clear();
        state.forced = false;
        state.debounce_deadline = None;
    }

    /// Mark the fetched page as prepared. This atomically removes matching
    /// events that arrived while the GraphQL request was in flight.
    pub(crate) fn set_active<I>(&self, ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut state = self.state();
        state.active_ids.clear();
        state.active_ids.extend(ids);
        let active_ids = state.active_ids.clone();
        state.pending_ids.retain(|id| !active_ids.contains(id));
    }

    /// Finish a non-empty batch and report whether new work arrived while it
    /// was being retrieved or processed.
    pub(crate) fn finish_batch(&self) -> bool {
        let mut state = self.state();
        state.active_ids.clear();
        state.forced || !state.pending_ids.is_empty()
    }

    /// Finish an empty fresh lookup. Forces or events that arrived after the
    /// lookup began are retained: a reconnect catch-up must not be consumed by
    /// a request that may have taken its snapshot before the reconnect.
    pub(crate) fn finish_empty_lookup(&self) {
        self.state().active_ids.clear();
    }
}

impl Default for WorkTrigger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn polls_every_six_seconds_while_pusher_is_unavailable() {
        let status = PusherStatus::new();
        let mut poller = status.poller();
        let waiting = tokio::spawn(async move { poller.tick().await });

        tokio::time::advance(PUSHER_OUTAGE_POLL_INTERVAL - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_connection_failures_do_not_postpone_fallback_polling() {
        let status = PusherStatus::new();
        let mut poller = status.poller();
        let waiting = tokio::spawn(async move { poller.tick().await });

        tokio::time::advance(Duration::from_secs(5)).await;
        status.set_connected(false);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;

        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_connected_subscription_uses_the_five_minute_safety_poll() {
        let status = PusherStatus::new();
        status.set_connected(true);
        let mut poller = status.poller();
        let waiting = tokio::spawn(async move { poller.tick().await });

        tokio::time::advance(SAFETY_POLL_INTERVAL - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn losing_pusher_restarts_the_timer_at_six_seconds() {
        let status = PusherStatus::new();
        status.set_connected(true);
        let mut poller = status.poller();
        let waiting = tokio::spawn(async move { poller.tick().await });

        tokio::time::advance(Duration::from_secs(100)).await;
        status.set_connected(false);
        tokio::task::yield_now().await;
        tokio::time::advance(PUSHER_OUTAGE_POLL_INTERVAL - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn restoring_pusher_restarts_the_five_minute_timer() {
        let status = PusherStatus::new();
        let mut poller = status.poller();
        let waiting = tokio::spawn(async move { poller.tick().await });

        tokio::time::advance(Duration::from_secs(5)).await;
        status.set_connected(true);
        tokio::task::yield_now().await;
        tokio::time::advance(SAFETY_POLL_INTERVAL - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn one_event_uses_a_trailing_500_millisecond_debounce() {
        let trigger = WorkTrigger::new();
        trigger.record_event("one".to_string());

        let waiting = tokio::spawn({
            let trigger = trigger.clone();
            async move { trigger.wait_until_ready().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        tokio::time::advance(Duration::from_millis(400)).await;
        trigger.record_event("two".to_string());
        tokio::time::advance(Duration::from_millis(499)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn twenty_five_unique_events_bypass_the_debounce() {
        let trigger = WorkTrigger::new();
        for n in 0..EVENT_TRIGGER_LIMIT - 1 {
            trigger.record_event(format!("event-{n}"));
        }

        let waiting = tokio::spawn({
            let trigger = trigger.clone();
            async move { trigger.wait_until_ready().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        // A duplicate does not count toward the threshold.
        trigger.record_event("event-0".to_string());
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        trigger.record_event("event-24".to_string());
        waiting.await.unwrap();
    }

    #[test]
    fn prepared_and_in_flight_events_do_not_force_a_follow_up() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();

        // This event races with the GraphQL response, then is found in it.
        trigger.record_event("prepared".to_string());
        trigger.set_active(["prepared".to_string()]);
        // A second delivery while processing is also a duplicate.
        trigger.record_event("prepared".to_string());

        assert!(!trigger.finish_batch());
    }

    #[test]
    fn a_new_event_during_processing_forces_a_follow_up() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();
        trigger.set_active(["prepared".to_string()]);
        trigger.record_event("new".to_string());

        assert!(trigger.finish_batch());
    }

    #[test]
    fn a_force_during_a_cursor_scan_survives_until_a_fresh_lookup() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();
        trigger.force();

        // Cursor continuation ended without another page. The force must
        // still restart the scan from cursor None.
        assert!(trigger.finish_batch());

        trigger.begin_fresh_lookup();
        trigger.finish_empty_lookup();
        let state = trigger.state();
        assert!(!state.forced);
        assert!(state.pending_ids.is_empty());
    }

    #[test]
    fn a_force_racing_with_an_empty_fresh_lookup_is_not_lost() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();
        trigger.force();
        trigger.finish_empty_lookup();

        assert!(trigger.state().forced);
    }
}
