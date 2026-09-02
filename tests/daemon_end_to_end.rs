//! End-to-end tests: the real daemon binary against a mock Enjin Platform.
//!
//! These cover what the unit tests structurally cannot — that the assembled
//! daemon actually fetches, signs and submits, and that its failure handling
//! paces requests instead of hammering a platform that is already struggling.

mod support;

use std::time::Duration;
use support::{Daemon, MockPlatform, PendingTx, PopulateBehaviour, SignBehaviour};

/// Generous, because a cold `cargo test` may still be linking the binary.
const BOOT: Duration = Duration::from_secs(30);
/// Long enough for the 1s/2s/4s/8s backoff to become unmistakable.
const OUTAGE_WINDOW: Duration = Duration::from_secs(20);

#[tokio::test]
async fn the_daemon_signs_a_pending_transaction_end_to_end() {
    let platform = MockPlatform::start().await;
    platform.set_tx_page(None, vec![PendingTx::good("tx-1")], None);

    let daemon = Daemon::start(&platform);

    platform
        .wait_for("the transaction to be signed", BOOT, |p| {
            !p.signed_uuids().is_empty()
        })
        .await;

    assert_eq!(
        platform.signed_uuids(),
        vec!["tx-1".to_string()],
        "logs:\n{}",
        daemon.dump_logs()
    );
}

#[tokio::test]
async fn every_page_of_a_multi_page_scan_is_signed_exactly_once() {
    // Pagination plus the producer/consumer ack: the same uuid must never be
    // handed to `SignTransactions` twice, or it is signed with two different
    // nonces and the platform rejects it.
    let platform = MockPlatform::start().await;
    platform.set_tx_page(
        None,
        (0..25)
            .map(|n| PendingTx::good(&format!("p1-{n}")))
            .collect(),
        Some("page-2"),
    );
    platform.set_tx_page(
        Some("page-2"),
        (0..25)
            .map(|n| PendingTx::good(&format!("p2-{n}")))
            .collect(),
        Some("page-3"),
    );
    platform.set_tx_page(
        Some("page-3"),
        (0..10)
            .map(|n| PendingTx::good(&format!("p3-{n}")))
            .collect(),
        None,
    );

    let daemon = Daemon::start(&platform);
    platform
        .wait_for("all 60 transactions to be signed", BOOT, |p| {
            p.signed_uuids().len() >= 60
        })
        .await;

    let mut signed = platform.signed_uuids();
    let total = signed.len();
    signed.sort();
    signed.dedup();
    assert_eq!(
        signed.len(),
        total,
        "a uuid was signed more than once; logs:\n{}",
        daemon.dump_logs()
    );
    assert_eq!(signed.len(), 60, "every pending transaction must be signed");
}

#[tokio::test]
async fn an_unsignable_row_does_not_starve_the_pages_behind_it() {
    // The original blocking defect: a page that could not be fully signed sent
    // the scan back to the head, so healthy work on later pages was never
    // reached.
    let platform = MockPlatform::start().await;
    platform.set_tx_page(
        None,
        vec![PendingTx::poison("poison"), PendingTx::good("early")],
        Some("page-2"),
    );
    platform.set_tx_page(Some("page-2"), vec![PendingTx::good("late")], None);

    let daemon = Daemon::start(&platform);
    platform
        .wait_for("the page behind the poison row to be reached", BOOT, |p| {
            p.signed_uuids().iter().any(|uuid| uuid == "late")
        })
        .await;

    let signed = platform.signed_uuids();
    assert!(
        signed.contains(&"early".to_string()),
        "the healthy row sharing the poison page must still be signed; logs:\n{}",
        daemon.dump_logs()
    );
    assert!(
        !signed.contains(&"poison".to_string()),
        "an unsignable row must never reach SignTransactions"
    );
}

#[tokio::test]
async fn a_platform_outage_is_retried_with_escalating_backoff_not_a_request_storm() {
    // During an outage the daemon still has work queued and nothing succeeds,
    // so this is exactly the state in which an unpaced retry loop would hammer
    // an already-failing platform. The backoff must escalate instead.
    let platform = MockPlatform::start().await;
    platform.set_tx_page(None, vec![PendingTx::good("tx-1")], None);
    platform.set_sign_behaviour(SignBehaviour::HttpError);
    platform.repeat_first_page(true);

    let daemon = Daemon::start(&platform);
    platform
        .wait_for("the first submission attempt", BOOT, |p| {
            p.count_of("SignTransactions") >= 1
        })
        .await;

    platform.reset_calls();
    tokio::time::sleep(OUTAGE_WINDOW).await;

    let attempts = platform.calls_to("SignTransactions");
    let gaps: Vec<f64> = attempts
        .windows(2)
        .map(|w| w[1].at.duration_since(w[0].at).as_secs_f64())
        .collect();

    assert!(
        attempts.len() <= 10,
        "{} submissions in {:?} is a request storm, not a backoff (gaps {gaps:?}); logs:\n{}",
        attempts.len(),
        OUTAGE_WINDOW,
        daemon.dump_logs(),
    );
    let longest = gaps.iter().cloned().fold(0.0_f64, f64::max);
    assert!(
        longest >= 5.0,
        "the backoff never escalated past {longest:.1}s (gaps {gaps:?}); logs:\n{}",
        daemon.dump_logs(),
    );
}

#[tokio::test]
async fn a_rejected_wallet_population_is_retried_rather_than_silently_dropped() {
    // `PopulateManagedWallets` returns `Boolean!`. A `false` is a business
    // rejection: treating it as success advances the cursor and logs "Updated
    // wallet" for wallets that were never populated.
    let platform = MockPlatform::start().await;
    platform.set_wallet_page(None, vec!["player-1", "player-2"], None);
    platform.set_populate_behaviour(PopulateBehaviour::RejectFalse);

    let daemon = Daemon::start(&platform);
    platform
        .wait_for("the rejection to be retried", BOOT, |p| {
            p.count_of("PopulateManagedWallets") >= 2
        })
        .await;

    assert!(
        !daemon.log_contains("Updated wallet (externalId: player-1)"),
        "a rejected population must not be reported as an update; logs:\n{}",
        daemon.dump_logs()
    );
    assert!(
        daemon.log_contains("Platform rejected PopulateManagedWallets"),
        "the rejection must be surfaced; logs:\n{}",
        daemon.dump_logs()
    );
}

#[tokio::test]
async fn a_server_that_repeats_its_cursor_does_not_trap_the_daemon() {
    // A page of rows the daemon cannot convert, whose `nextCursor` never
    // advances. Draining past unconvertible rows must not become an unpaced
    // loop over the same page.
    let platform = MockPlatform::start().await;
    // Every request answers with the same non-empty cursor, so following it
    // never makes progress.
    platform.set_tx_page(None, vec![PendingTx::unconvertible("bad")], Some("stuck"));
    platform.set_tx_page(
        Some("stuck"),
        vec![PendingTx::unconvertible("bad")],
        Some("stuck"),
    );

    let daemon = Daemon::start(&platform);
    // Anchor the measurement to observed daemon activity. A bare sleep plus an
    // upper bound passes when the daemon never starts at all — a slow boot, a
    // crash, or a missing env var all yield zero lookups, which trivially
    // satisfies `<= 12` and silently disarms this guard.
    platform
        .wait_for("the first lookup", BOOT, |p| {
            p.count_of("GetPendingTransactions") >= 1
        })
        .await;
    platform.reset_calls();
    tokio::time::sleep(Duration::from_secs(6)).await;

    let lookups = platform.count_of("GetPendingTransactions");
    assert!(
        lookups >= 2,
        "the daemon stopped scanning entirely ({lookups} lookups in 6s); logs:\n{}",
        daemon.dump_logs()
    );
    assert!(
        lookups <= 12,
        "{lookups} lookups in 6s means the daemon is looping on a repeated cursor; logs:\n{}",
        daemon.dump_logs()
    );
}

#[tokio::test]
async fn a_repeated_cursor_on_an_unsignable_page_does_not_trap_the_daemon() {
    // The sibling of the test above for the *other* way a page can produce
    // nothing: rows that convert fine but can never be signed. That path
    // reaches `BatchOutcome::Skipped` rather than the empty-page drain, and
    // needs the same cursor-advance and page bounds.
    let platform = MockPlatform::start().await;
    platform.set_tx_page(None, vec![PendingTx::poison("bad")], Some("stuck"));
    platform.set_tx_page(Some("stuck"), vec![PendingTx::poison("bad")], Some("stuck"));

    let daemon = Daemon::start(&platform);
    platform
        .wait_for("the first lookup", BOOT, |p| {
            p.count_of("GetPendingTransactions") >= 1
        })
        .await;
    platform.reset_calls();
    tokio::time::sleep(Duration::from_secs(6)).await;

    let lookups = platform.count_of("GetPendingTransactions");
    assert!(
        lookups >= 2,
        "the daemon stopped scanning entirely ({lookups} lookups in 6s); logs:\n{}",
        daemon.dump_logs()
    );
    assert!(
        lookups <= 12,
        "{lookups} lookups in 6s means the daemon is looping on a repeated cursor; logs:\n{}",
        daemon.dump_logs()
    );
}
