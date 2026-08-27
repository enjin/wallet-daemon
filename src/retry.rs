use rand::RngExt;
use std::time::Duration;

const INITIAL_BACKOFF_SECS: u64 = 1;
const MAX_BACKOFF_SECS: u64 = 60;

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
    fn jitter_stays_within_twenty_percent_of_the_capped_delay() {
        for _ in 0..100 {
            let delay = jittered_exponential_delay(6);
            assert!(delay >= Duration::from_secs(48));
            assert!(delay <= Duration::from_secs(60));
        }
    }
}
