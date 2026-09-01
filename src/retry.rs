use rand::RngExt;
use std::time::Duration;

const INITIAL_BACKOFF_SECS: u64 = 1;
const MAX_BACKOFF_SECS: u64 = 60;

/// Upper bound on consecutive pages containing no convertible rows that a
/// single cursor scan will drain without pausing.
pub(crate) const MAX_CONSECUTIVE_EMPTY_PAGES: u32 = 20;

/// Whether a cursor scan should keep draining past a page that yielded no
/// usable rows.
///
/// Skipping such a page matters: dropping the cursor there lets malformed data
/// at the head of the queue hide every healthy page behind it. But the drain
/// runs with no delay between requests, so it needs two bounds — the cursor
/// must actually move (a server that echoes the same cursor would otherwise
/// loop forever), and a single scan may only skip so many pages before falling
/// back to the normal backoff.
pub(crate) fn should_continue_empty_drain(
    cursor: Option<&str>,
    next_cursor: Option<&str>,
    empty_pages: u32,
) -> bool {
    match next_cursor {
        Some(next) => {
            next_cursor != cursor && empty_pages < MAX_CONSECUTIVE_EMPTY_PAGES && !next.is_empty()
        }
        None => false,
    }
}

/// Return a 1, 2, 4, ... second delay capped at one minute.
pub(crate) fn exponential_delay(failure_count: u32) -> Duration {
    let multiplier = 1u64.checked_shl(failure_count.min(63)).unwrap_or(u64::MAX);
    Duration::from_secs(
        INITIAL_BACKOFF_SECS
            .saturating_mul(multiplier)
            .min(MAX_BACKOFF_SECS),
    )
}

/// Apply +/-20% jitter to the capped exponential delay.
pub(crate) fn jittered_exponential_delay(failure_count: u32) -> Duration {
    let base_ms = exponential_delay(failure_count).as_millis() as u64;
    let jitter_percent = rand::rng().random_range(80u64..=120);
    Duration::from_millis(
        (base_ms.saturating_mul(jitter_percent) / 100).min(MAX_BACKOFF_SECS * 1_000),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_delay_starts_at_one_second_and_caps_at_one_minute() {
        assert_eq!(exponential_delay(0), Duration::from_secs(1));
        assert_eq!(exponential_delay(1), Duration::from_secs(2));
        assert_eq!(exponential_delay(5), Duration::from_secs(32));
        assert_eq!(exponential_delay(6), Duration::from_secs(60));
        assert_eq!(exponential_delay(100), Duration::from_secs(60));
    }

    #[test]
    fn empty_drain_stops_when_the_scan_has_ended() {
        assert!(!should_continue_empty_drain(Some("a"), None, 0));
    }

    #[test]
    fn empty_drain_continues_while_the_cursor_advances() {
        assert!(should_continue_empty_drain(None, Some("page-2"), 0));
        assert!(should_continue_empty_drain(
            Some("page-1"),
            Some("page-2"),
            5
        ));
    }

    #[test]
    fn empty_drain_stops_when_the_server_repeats_the_cursor() {
        // Without this the loop re-requests the same page forever with no
        // delay between requests.
        assert!(!should_continue_empty_drain(
            Some("page-1"),
            Some("page-1"),
            0
        ));
        assert!(!should_continue_empty_drain(None, Some(""), 0));
    }

    #[test]
    fn empty_drain_stops_after_the_consecutive_page_limit() {
        assert!(should_continue_empty_drain(
            Some("a"),
            Some("b"),
            MAX_CONSECUTIVE_EMPTY_PAGES - 1
        ));
        assert!(!should_continue_empty_drain(
            Some("a"),
            Some("b"),
            MAX_CONSECUTIVE_EMPTY_PAGES
        ));
    }

    #[test]
    fn jitter_stays_within_twenty_percent_of_the_capped_delay() {
        for _ in 0..100 {
            let delay = jittered_exponential_delay(6);
            assert!(delay >= Duration::from_secs(48));
            assert!(delay <= Duration::from_secs(60));
        }
    }
}
