use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tokio::time::{Instant, sleep_until};

pub(crate) const EVENT_TRIGGER_LIMIT: usize = 25;
pub(crate) const EVENT_DEBOUNCE: Duration = Duration::from_millis(500);
pub(crate) const SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(180);
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
    /// Monotonically identifies the latest notification observed. A fresh
    /// lookup captures this generation so notifications received while it is
    /// in flight can schedule a follow-up without retaining event payloads.
    event_generation: u64,
    lookup_generation: Option<u64>,
    /// Number of notifications waiting to be covered by a fresh lookup. The
    /// count is capped because only the 25-delivery fast path is significant.
    pending_event_count: usize,
    /// Startup, reconnect, or another non-event reason to perform a lookup.
    forced: bool,
    /// Trailing-edge debounce deadline for pending notifications while idle.
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

    /// Record one platform notification without retaining its payload.
    pub(crate) fn record_event(&self) {
        {
            let mut state = self.state();
            state.event_generation = state.event_generation.wrapping_add(1);
            state.pending_event_count = state
                .pending_event_count
                .saturating_add(1)
                .min(EVENT_TRIGGER_LIMIT);
            state.debounce_deadline = Some(Instant::now() + EVENT_DEBOUNCE);
        }
        self.inner.notify.notify_one();
    }

    /// Force a lookup for startup or after a successful WebSocket reconnect.
    pub(crate) fn force(&self) {
        self.state().forced = true;
        self.inner.notify.notify_one();
    }

    /// Wait for a force, 25 event deliveries, or the trailing debounce deadline.
    pub(crate) async fn wait_until_ready(&self) {
        loop {
            // Register before checking state so a notification racing with the
            // check is retained by `Notify` instead of being lost.
            let notified = self.inner.notify.notified();
            let deadline = {
                let state = self.state();
                if state.forced
                    || state.pending_event_count >= EVENT_TRIGGER_LIMIT
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

    /// Wait for a notification that arrives strictly *after* this call.
    ///
    /// Unlike [`WorkTrigger::wait_until_ready`], already-pending state does
    /// not satisfy this. `wait_until_ready` is a pure observer of state that
    /// only `begin_fresh_lookup` clears, so using it to shorten a failure
    /// backoff lets work the caller already knows about pre-empt the delay
    /// on every iteration — turning a failing platform into an unpaced
    /// request storm. A failure delay must be honoured against known work
    /// and interrupted only by something genuinely new.
    pub(crate) async fn wait_for_new_event(&self) {
        let (baseline_generation, baseline_forced) = {
            let state = self.state();
            (state.event_generation, state.forced)
        };

        loop {
            // Register before checking state so a notification racing with
            // the check is retained by `Notify` instead of being lost.
            let notified = self.inner.notify.notified();
            {
                let state = self.state();
                if state.event_generation != baseline_generation
                    || (state.forced && !baseline_forced)
                {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Start a lookup from `cursor: None`. Notifications already pending are
    /// covered by this authoritative scan. A generation change after this
    /// call forces another scan after the current work finishes.
    pub(crate) fn begin_fresh_lookup(&self) {
        let mut state = self.state();
        state.lookup_generation = Some(state.event_generation);
        state.pending_event_count = 0;
        state.forced = false;
        state.debounce_deadline = None;
    }

    /// Finish a non-empty batch and report whether new work arrived while the
    /// cursor scan was being retrieved or processed.
    pub(crate) fn finish_batch(&self, scan_complete: bool) -> bool {
        let mut state = self.state();
        let deferred = state.forced
            || state.pending_event_count > 0
            || state
                .lookup_generation
                .is_some_and(|generation| generation != state.event_generation);
        if scan_complete {
            state.lookup_generation = None;
        }
        deferred
    }

    /// Finish an empty fresh lookup. Forces and notifications that arrived
    /// after it began remain pending for the next lookup.
    pub(crate) fn finish_empty_lookup(&self) {
        self.state().lookup_generation = None;
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
    use tokio::time::timeout;

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
    async fn a_connected_subscription_uses_the_safety_poll() {
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
    async fn restoring_pusher_restarts_the_safety_poll_timer() {
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
        trigger.record_event();

        let waiting = tokio::spawn({
            let trigger = trigger.clone();
            async move { trigger.wait_until_ready().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        tokio::time::advance(Duration::from_millis(400)).await;
        trigger.record_event();
        tokio::time::advance(Duration::from_millis(499)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn twenty_five_event_deliveries_bypass_the_debounce() {
        let trigger = WorkTrigger::new();
        for _ in 0..EVENT_TRIGGER_LIMIT - 1 {
            trigger.record_event();
        }

        let waiting = tokio::spawn({
            let trigger = trigger.clone();
            async move { trigger.wait_until_ready().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        trigger.record_event();
        waiting.await.unwrap();
    }

    #[test]
    fn events_known_when_lookup_begins_are_covered_by_that_lookup() {
        let trigger = WorkTrigger::new();
        trigger.record_event();

        trigger.begin_fresh_lookup();
        trigger.finish_empty_lookup();
        let state = trigger.state();
        assert_eq!(state.event_generation, 1);
        assert_eq!(state.pending_event_count, 0);
        assert!(state.lookup_generation.is_none());
    }

    #[test]
    fn event_counter_is_capped_at_the_trigger_limit() {
        let trigger = WorkTrigger::new();
        for _ in 0..EVENT_TRIGGER_LIMIT * 4 {
            trigger.record_event();
        }

        let state = trigger.state();
        assert_eq!(state.pending_event_count, EVENT_TRIGGER_LIMIT);
        assert_eq!(state.event_generation, (EVENT_TRIGGER_LIMIT * 4) as u64);
    }

    #[test]
    fn no_event_during_a_lookup_requires_no_follow_up() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();

        assert!(!trigger.finish_batch(true));
    }

    #[test]
    fn a_new_event_during_processing_forces_a_follow_up() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();
        trigger.record_event();

        assert!(trigger.finish_batch(true));
    }

    #[test]
    fn an_event_during_a_cursor_scan_survives_until_scan_completion() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();
        trigger.record_event();

        assert!(trigger.finish_batch(false));
        assert!(trigger.state().lookup_generation.is_some());
        assert!(trigger.finish_batch(true));
        assert!(trigger.state().lookup_generation.is_none());
    }

    #[test]
    fn an_event_racing_with_an_empty_lookup_remains_pending() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();
        trigger.record_event();
        trigger.finish_empty_lookup();

        let state = trigger.state();
        assert_eq!(state.pending_event_count, 1);
        assert!(state.lookup_generation.is_none());
    }

    #[test]
    fn a_force_during_a_cursor_scan_survives_until_a_fresh_lookup() {
        let trigger = WorkTrigger::new();
        trigger.begin_fresh_lookup();
        trigger.force();

        // Cursor continuation ended without another page. The force must
        // still restart the scan from cursor None.
        assert!(trigger.finish_batch(true));

        trigger.begin_fresh_lookup();
        trigger.finish_empty_lookup();
        let state = trigger.state();
        assert!(!state.forced);
        assert_eq!(state.pending_event_count, 0);
        assert!(state.lookup_generation.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn already_pending_work_does_not_satisfy_wait_for_new_event() {
        // This is what keeps a failure backoff paced. `wait_until_ready` is a
        // pure observer of state that only `begin_fresh_lookup` clears, so
        // using it to shorten a retry delay lets work we already know about
        // cancel the delay on every iteration.
        let trigger = WorkTrigger::new();
        trigger.record_event();

        // `wait_until_ready` is satisfied immediately by the pending event...
        tokio::time::advance(EVENT_DEBOUNCE).await;
        timeout(Duration::from_millis(10), trigger.wait_until_ready())
            .await
            .expect("pending work satisfies wait_until_ready");

        // ...but the retry wait is not.
        let waiting = tokio::spawn({
            let trigger = trigger.clone();
            async move { trigger.wait_for_new_event().await }
        });
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "work pending before the delay began must not cancel it",
        );

        // Only something genuinely new releases it.
        trigger.record_event();
        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_force_during_the_delay_satisfies_wait_for_new_event() {
        let trigger = WorkTrigger::new();
        let waiting = tokio::spawn({
            let trigger = trigger.clone();
            async move { trigger.wait_for_new_event().await }
        });
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        trigger.force();
        waiting.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_force_already_set_does_not_satisfy_wait_for_new_event() {
        // A reconnect force that is still outstanding is exactly the work the
        // failing scan could not complete; it must not cancel the backoff.
        let trigger = WorkTrigger::new();
        trigger.force();

        let waiting = tokio::spawn({
            let trigger = trigger.clone();
            async move { trigger.wait_for_new_event().await }
        });
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        trigger.record_event();
        waiting.await.unwrap();
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
