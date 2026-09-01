use crate::graphql::populate_managed_wallets::PopulateManagedWalletInput;
use crate::graphql::{GetPendingManagedWalletCreations, get_pending_managed_wallet_creations};
use crate::work_trigger::{PusherStatus, WorkTrigger};
use crate::{platform_client, utils};
use subxt_signer::DeriveJunction;
use subxt_signer::sr25519::Keypair;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::sleep;

const ACCOUNT_PAGE_SIZE: i64 = 100;

#[derive(Clone)]
pub struct DeriveWalletRequest {
    external_id: String,
}

impl TryFrom<get_pending_managed_wallet_creations::GetPendingManagedWalletCreationsResultData>
    for DeriveWalletRequest
{
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn try_from(
        data: get_pending_managed_wallet_creations::GetPendingManagedWalletCreationsResultData,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            external_id: data.external_id.ok_or("No external id")?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BatchOutcome {
    Completed,
    SubmissionFailed,
}

/// What the producer should do after one page of a managed-wallet scan.
///
/// Kept separate from the loop so the pagination, backoff and restart rules are
/// decided by a pure function that can be asserted directly, rather than only
/// being reachable through a live GraphQL round trip.
#[derive(Debug, Eq, PartialEq)]
enum WalletStep {
    /// Carry on with no failure delay.
    Continue {
        cursor: Option<String>,
        restart_after_scan: bool,
        reset_failures: bool,
        fetch_now: bool,
    },
    /// Wait out the failure backoff, then issue the next request.
    DelayThenFetch {
        cursor: Option<String>,
        restart_after_scan: bool,
        reason: &'static str,
    },
}

/// Decide the next step after a page that produced wallets to populate.
fn wallet_step(
    outcome: BatchOutcome,
    next_cursor: Option<String>,
    restart_after_scan: bool,
    deferred: bool,
) -> WalletStep {
    match outcome {
        // The scan finished, but an earlier page left work behind: pace the
        // restart rather than re-scanning immediately.
        BatchOutcome::Completed if next_cursor.is_none() && restart_after_scan => {
            WalletStep::DelayThenFetch {
                cursor: None,
                restart_after_scan: false,
                reason: "Managed-wallet scan left pending work",
            }
        }
        BatchOutcome::Completed => WalletStep::Continue {
            // A later healthy page must not clear the backoff for a scan that
            // an earlier page already marked for retry.
            reset_failures: !restart_after_scan,
            fetch_now: next_cursor.is_some() || deferred,
            cursor: next_cursor,
            restart_after_scan,
        },
        // Keep the scan moving so a poison page cannot hide valid wallets on
        // every later page, but always pace the next request.
        BatchOutcome::SubmissionFailed => WalletStep::DelayThenFetch {
            restart_after_scan: next_cursor.is_some(),
            cursor: next_cursor,
            reason: "Managed-wallet batch submission failed",
        },
    }
}

/// Map an external id onto the derivation junction for its managed wallet.
///
/// Numeric ids derive from the integer, everything else from the string. The
/// two produce different addresses for the same text, so this mapping is part
/// of the wallet identity contract: changing it silently repoints every
/// managed wallet the platform has already been told about.
fn derive_junction(external_id: &str) -> DeriveJunction {
    match external_id.parse::<i64>() {
        Ok(number) => DeriveJunction::soft(number),
        Err(_) => DeriveJunction::soft(external_id),
    }
}

/// Decide the next step after a scan ended with nothing left to populate.
fn empty_scan_step(restart_after_scan: bool, deferred: bool) -> WalletStep {
    if restart_after_scan {
        WalletStep::DelayThenFetch {
            cursor: None,
            restart_after_scan: false,
            reason: "Managed-wallet scan skipped unconvertible rows",
        }
    } else {
        WalletStep::Continue {
            cursor: None,
            restart_after_scan: false,
            reset_failures: true,
            fetch_now: deferred,
        }
    }
}

struct WalletBatch {
    requests: Vec<DeriveWalletRequest>,
    ack: oneshot::Sender<BatchOutcome>,
}

#[derive(Debug)]
pub struct DeriveWalletJob {
    sender: Sender<WalletBatch>,
    trigger: WorkTrigger,
    pusher_status: PusherStatus,
}

impl DeriveWalletJob {
    pub fn create_job(
        keypair: Keypair,
        trigger: WorkTrigger,
        pusher_status: PusherStatus,
    ) -> (DeriveWalletJob, DeriveWalletProcessor) {
        // Capacity one plus the per-batch acknowledgement guarantees that a
        // wallet page is fully populated before another page is fetched.
        let (sender, receiver) = tokio::sync::mpsc::channel(1);

        (
            DeriveWalletJob {
                sender,
                trigger,
                pusher_status,
            },
            DeriveWalletProcessor::new(keypair, receiver),
        )
    }

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(error) = self.start_polling().await {
                tracing::error!("Managed-wallet worker exiting due to fatal error: {error}");
            }
        })
    }

    async fn start_polling(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut cursor: Option<String> = None;
        let mut fetch_now = true; // Mandatory startup catch-up.
        let mut failure_count = 0u32;
        let mut restart_after_scan = false;
        // Consecutive pages in the current scan that contained no convertible
        // rows. Bounds the unpaced drain past malformed data.
        let mut empty_pages = 0u32;
        let mut pusher_aware_poll = self.pusher_status.poller();

        loop {
            if !fetch_now {
                tokio::select! {
                    _ = self.trigger.wait_until_ready() => {}
                    _ = pusher_aware_poll.tick() => {}
                }
            }

            let fresh_lookup = cursor.is_none();
            if fresh_lookup {
                self.trigger.begin_fresh_lookup();
            }

            // Every arm below assigns `fetch_now`, either directly or through
            // `apply_step`.
            match self.get_pending_wallets(cursor.clone()).await {
                Ok((requests, next_cursor)) if requests.is_empty() => {
                    // Keep draining past a page that contained only malformed
                    // rows so later valid wallets are not starved behind it —
                    // but only while the cursor actually advances, and only
                    // for a bounded run, so a large block of malformed rows
                    // (or a server that returns the same cursor) cannot become
                    // an unpaced request storm.
                    if crate::retry::should_continue_empty_drain(
                        cursor.as_deref(),
                        next_cursor.as_deref(),
                        empty_pages,
                    ) {
                        empty_pages += 1;
                        cursor = next_cursor;
                        restart_after_scan = true;
                        fetch_now = true;
                    } else {
                        if next_cursor.is_some() {
                            tracing::warn!(
                                "Abandoning managed-wallet scan after {empty_pages} page(s) containing no convertible rows; restarting from a fresh lookup",
                            );
                            restart_after_scan = true;
                        }
                        empty_pages = 0;
                        let deferred = if fresh_lookup {
                            self.trigger.finish_empty_lookup();
                            false
                        } else {
                            self.trigger.finish_batch(true)
                        };
                        tracing::info!("No pending managed wallets");

                        let step = empty_scan_step(restart_after_scan, deferred);
                        (cursor, restart_after_scan, fetch_now) =
                            self.apply_step(step, &mut failure_count).await;
                    }
                }
                Ok((requests, next_cursor)) => {
                    empty_pages = 0;
                    let scan_complete = next_cursor.is_none();
                    let outcome = self.send_batch_and_wait(requests).await?;
                    let deferred = self.trigger.finish_batch(scan_complete);

                    let step = wallet_step(outcome, next_cursor, restart_after_scan, deferred);
                    (cursor, restart_after_scan, fetch_now) =
                        self.apply_step(step, &mut failure_count).await;
                }
                Err(error) => {
                    cursor = None;
                    restart_after_scan = false;
                    empty_pages = 0;
                    self.trigger.finish_empty_lookup();
                    let delay = crate::retry::jittered_exponential_delay(failure_count);
                    failure_count = failure_count.saturating_add(1);
                    tracing::error!("GetPendingManagedWalletCreations failed: {error}");
                    tracing::warn!(
                        "Retrying managed-wallet lookup in {:.1}s",
                        delay.as_secs_f64(),
                    );
                    self.wait_for_retry_or_trigger(delay).await;
                    fetch_now = true;
                }
            }
        }
    }

    /// Carry out a decided [`WalletStep`], returning the loop's next
    /// `(cursor, restart_after_scan, fetch_now)`.
    async fn apply_step(
        &self,
        step: WalletStep,
        failure_count: &mut u32,
    ) -> (Option<String>, bool, bool) {
        match step {
            WalletStep::Continue {
                cursor,
                restart_after_scan,
                reset_failures,
                fetch_now,
            } => {
                if reset_failures {
                    *failure_count = 0;
                }
                (cursor, restart_after_scan, fetch_now)
            }
            WalletStep::DelayThenFetch {
                cursor,
                restart_after_scan,
                reason,
            } => {
                let delay = crate::retry::jittered_exponential_delay(*failure_count);
                *failure_count = failure_count.saturating_add(1);
                tracing::warn!("{reason}; continuing lookup in {:.1}s", delay.as_secs_f64());
                self.wait_for_retry_or_trigger(delay).await;
                (cursor, restart_after_scan, true)
            }
        }
    }

    /// Wait out a failure delay. Work that arrives *during* the delay cuts it
    /// short; work that was already pending when the failure occurred does
    /// not, so a failing platform can never be turned into an unpaced storm
    /// of requests.
    async fn wait_for_retry_or_trigger(&self, delay: std::time::Duration) {
        tokio::select! {
            _ = sleep(delay) => {}
            _ = self.trigger.wait_for_new_event() => {
                tracing::info!("New managed-wallet work interrupted the retry delay");
            }
        }
    }

    async fn send_batch_and_wait(
        &self,
        requests: Vec<DeriveWalletRequest>,
    ) -> Result<BatchOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let (ack, completion) = oneshot::channel();
        self.sender
            .send(WalletBatch { requests, ack })
            .await
            .map_err(|_| "managed-wallet processor receiver dropped")?;
        completion
            .await
            .map_err(|_| "managed-wallet processor dropped batch acknowledgement".into())
    }

    async fn get_pending_wallets(
        &self,
        cursor: Option<String>,
    ) -> Result<(Vec<DeriveWalletRequest>, Option<String>), Box<dyn std::error::Error + Send + Sync>>
    {
        let response_data = utils::execute_query::<GetPendingManagedWalletCreations>(
            get_pending_managed_wallet_creations::Variables {
                limit: ACCOUNT_PAGE_SIZE,
                cursor,
            },
            None,
        )
        .await?;

        let Some(result) = response_data.result else {
            return Ok((Vec::new(), None));
        };
        let next_cursor = result.next_cursor.filter(|cursor| !cursor.is_empty());
        let requests = result
            .data
            .into_iter()
            .filter_map(|pending| {
                DeriveWalletRequest::try_from(pending)
                    .map_err(|error| {
                        tracing::error!("Error creating DeriveWalletRequest: {error}");
                        error
                    })
                    .ok()
            })
            .collect();

        Ok((requests, next_cursor))
    }
}

pub struct DeriveWalletProcessor {
    keypair: Keypair,
    receiver: Receiver<WalletBatch>,
}

impl DeriveWalletProcessor {
    fn new(keypair: Keypair, receiver: Receiver<WalletBatch>) -> Self {
        Self { keypair, receiver }
    }

    async fn derive_wallets(keypair: Keypair, requests: Vec<DeriveWalletRequest>) -> BatchOutcome {
        let wallets: Vec<_> = requests
            .into_iter()
            .map(|request| {
                let external_id = request.external_id;
                let derive_junction = derive_junction(&external_id);

                let derived_pair = keypair.derive([derive_junction]);
                let signature = {
                    let daemon_public_key = hex::encode(keypair.public_key().0);
                    let message =
                        format!("EnjinPlatform.VerifyManagedWallet(0x{daemon_public_key})");
                    derived_pair.sign(message.as_bytes())
                };
                PopulateManagedWalletInput {
                    external_id,
                    public_key: format!("0x{}", hex::encode(derived_pair.public_key().0)),
                    signed_message: format!("0x{}", hex::encode(signature.0)),
                }
            })
            .collect();

        match platform_client::populate_managed_wallets(wallets).await {
            Ok(()) => BatchOutcome::Completed,
            Err(error) => {
                tracing::error!("PopulateManagedWallets failed: {error}");
                BatchOutcome::SubmissionFailed
            }
        }
    }

    async fn launch_job_scheduler(mut self) {
        // Wallet batches are independent from transaction batches, but each
        // wallet worker remains strictly serial within its own workflow.
        while let Some(WalletBatch { requests, ack }) = self.receiver.recv().await {
            let outcome = Self::derive_wallets(self.keypair.clone(), requests).await;
            let _ = ack.send(outcome);
        }
    }

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(self.launch_job_scheduler())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(cursor: &str) -> Option<String> {
        Some(cursor.to_string())
    }

    // --- pagination: a poison page must not hide later pages ---

    #[test]
    fn a_failed_submission_keeps_draining_the_remaining_pages() {
        // The whole point of the four-state rework: a page the platform
        // rejects must not send the scan back to the head, or the rows behind
        // it are never reached.
        assert_eq!(
            wallet_step(BatchOutcome::SubmissionFailed, page("page-2"), false, false),
            WalletStep::DelayThenFetch {
                cursor: page("page-2"),
                restart_after_scan: true,
                reason: "Managed-wallet batch submission failed",
            }
        );
    }

    #[test]
    fn a_failed_submission_on_the_final_page_restarts_from_the_head() {
        assert_eq!(
            wallet_step(BatchOutcome::SubmissionFailed, None, false, false),
            WalletStep::DelayThenFetch {
                cursor: None,
                restart_after_scan: false,
                reason: "Managed-wallet batch submission failed",
            }
        );
    }

    #[test]
    fn a_completed_page_continues_the_scan_without_a_delay() {
        assert_eq!(
            wallet_step(BatchOutcome::Completed, page("page-2"), false, false),
            WalletStep::Continue {
                cursor: page("page-2"),
                restart_after_scan: false,
                reset_failures: true,
                fetch_now: true,
            }
        );
    }

    #[test]
    fn a_completed_final_page_parks_until_the_next_trigger() {
        assert_eq!(
            wallet_step(BatchOutcome::Completed, None, false, false),
            WalletStep::Continue {
                cursor: None,
                restart_after_scan: false,
                reset_failures: true,
                fetch_now: false,
            }
        );
    }

    #[test]
    fn work_arriving_during_the_scan_re_scans_immediately() {
        let step = wallet_step(BatchOutcome::Completed, None, false, true);
        let WalletStep::Continue { fetch_now, .. } = step else {
            panic!("a completed final page must not delay");
        };
        assert!(fetch_now, "a notification during the scan must be covered");
    }

    // --- backoff escalation must survive a healthy later page ---

    #[test]
    fn a_completed_page_does_not_clear_the_backoff_owed_by_the_scan() {
        // Resetting here is what pinned a poison-row scan at a ~1s retry
        // forever: every scan contained one healthy page.
        let step = wallet_step(BatchOutcome::Completed, page("page-3"), true, false);
        let WalletStep::Continue {
            reset_failures,
            restart_after_scan,
            ..
        } = step
        else {
            panic!("a completed mid-scan page must continue");
        };
        assert!(!reset_failures, "the owed retry must keep its backoff");
        assert!(restart_after_scan, "the owed retry must survive the page");
    }

    #[test]
    fn a_scan_that_owes_a_retry_pauses_before_restarting() {
        assert_eq!(
            wallet_step(BatchOutcome::Completed, None, true, false),
            WalletStep::DelayThenFetch {
                cursor: None,
                restart_after_scan: false,
                reason: "Managed-wallet scan left pending work",
            }
        );
    }

    #[test]
    fn backoff_escalates_across_a_multi_page_scan_with_one_bad_page() {
        // page 1 fails, pages 2 and 3 succeed, and the scan then restarts.
        // The restart must be paced by the failure, not reset by the successes.
        let mut restart_after_scan = false;
        let mut delays = 0;

        for (outcome, next) in [
            (BatchOutcome::SubmissionFailed, page("page-2")),
            (BatchOutcome::Completed, page("page-3")),
            (BatchOutcome::Completed, None),
        ] {
            match wallet_step(outcome, next, restart_after_scan, false) {
                WalletStep::Continue {
                    restart_after_scan: next_restart,
                    reset_failures,
                    ..
                } => {
                    assert!(!reset_failures, "a scan owing a retry keeps its backoff");
                    restart_after_scan = next_restart;
                }
                WalletStep::DelayThenFetch {
                    restart_after_scan: next_restart,
                    ..
                } => {
                    delays += 1;
                    restart_after_scan = next_restart;
                }
            }
        }

        assert_eq!(delays, 2, "the failed page and the restart are both paced");
        assert!(!restart_after_scan, "the restart clears the owed retry");
    }

    // --- scans that produced nothing ---

    #[test]
    fn an_empty_scan_with_nothing_skipped_resets_the_backoff() {
        assert_eq!(
            empty_scan_step(false, false),
            WalletStep::Continue {
                cursor: None,
                restart_after_scan: false,
                reset_failures: true,
                fetch_now: false,
            }
        );
    }

    #[test]
    fn an_empty_scan_that_skipped_rows_pauses_before_retrying() {
        assert_eq!(
            empty_scan_step(true, false),
            WalletStep::DelayThenFetch {
                cursor: None,
                restart_after_scan: false,
                reason: "Managed-wallet scan skipped unconvertible rows",
            }
        );
    }

    #[test]
    fn an_empty_scan_honours_a_notification_that_arrived_during_it() {
        let WalletStep::Continue { fetch_now, .. } = empty_scan_step(false, true) else {
            panic!("an empty scan with nothing skipped must not delay");
        };
        assert!(fetch_now);
    }

    // --- wallet identity derivation ---

    /// The substrate development mnemonic. Its root key is the well-known
    /// `//Alice`-family seed, so these vectors are reproducible anywhere.
    const TEST_MNEMONIC: &str =
        "bottom drive obey lake curtain smoke basket hold race lonely fit walk";

    fn test_keypair() -> Keypair {
        use std::str::FromStr;
        let uri = subxt_signer::SecretUri::from_str(TEST_MNEMONIC).unwrap();
        Keypair::from_uri(&uri).unwrap()
    }

    fn derived_public_key(external_id: &str) -> String {
        let derived = test_keypair().derive([derive_junction(external_id)]);
        format!("0x{}", hex::encode(derived.public_key().0))
    }

    #[test]
    fn managed_wallet_addresses_are_stable() {
        // These are the addresses the platform has already been told about for
        // these external ids. A change here silently repoints existing managed
        // wallets, so treat a failure as a compatibility break, not a stale
        // fixture.
        assert_eq!(
            derived_public_key("42"),
            "0x34def2da0cdcecb5907b5ba9749707d4e36114b9258fc77c51fb14922ce1496a",
        );
        assert_eq!(
            derived_public_key("wallet-1"),
            "0x464b92b868cf07a0adc20eeeff09c72ebb4636d317e49f79243fe3b0d3476505",
        );
    }

    #[test]
    fn numeric_and_textual_external_ids_derive_differently() {
        // "42" parses as an integer and derives from the number; "wallet-1"
        // does not and derives from the string. Collapsing the two cases would
        // move every numerically-named wallet.
        assert_ne!(derive_junction("42"), derive_junction("wallet-1"));
        assert_eq!(derive_junction("42"), DeriveJunction::soft(42i64));
        assert_eq!(
            derive_junction("wallet-1"),
            DeriveJunction::soft("wallet-1")
        );
    }

    #[test]
    fn an_out_of_range_numeric_id_falls_back_to_string_derivation() {
        // Larger than i64::MAX: parsing fails, so it must derive as text
        // rather than panic or silently truncate.
        let huge = "99999999999999999999999";
        assert_eq!(derive_junction(huge), DeriveJunction::soft(huge));
        assert!(!derived_public_key(huge).is_empty());
    }

    #[test]
    fn a_derived_wallet_is_not_the_daemon_key_itself() {
        assert_ne!(
            derived_public_key("42"),
            format!("0x{}", hex::encode(test_keypair().public_key().0)),
        );
    }

    // --- producer/consumer handshake ---

    #[tokio::test]
    async fn a_dropped_processor_is_a_fatal_error_rather_than_a_silent_stall() {
        // `send_batch_and_wait` is the only backpressure point: if the
        // processor is gone the producer must surface it so the daemon exits
        // and is restarted, not spin fetching pages nobody will populate.
        let (job, processor) =
            DeriveWalletJob::create_job(test_keypair(), WorkTrigger::new(), PusherStatus::new());
        drop(processor);

        let error = job
            .send_batch_and_wait(vec![DeriveWalletRequest {
                external_id: "wallet-1".to_string(),
            }])
            .await
            .expect_err("a dropped processor must not look like success");
        assert!(error.to_string().contains("receiver dropped"), "{error}");
    }

    #[tokio::test]
    async fn a_processor_that_abandons_a_batch_is_a_fatal_error() {
        // The ack is dropped without a value, i.e. the processor panicked
        // mid-batch. The producer must not wait forever for it.
        let (job, mut processor) =
            DeriveWalletJob::create_job(test_keypair(), WorkTrigger::new(), PusherStatus::new());

        let abandoning = tokio::spawn(async move {
            let batch = processor.receiver.recv().await.expect("a batch is sent");
            drop(batch); // drops the ack sender without signalling
        });

        let error = job
            .send_batch_and_wait(vec![DeriveWalletRequest {
                external_id: "wallet-1".to_string(),
            }])
            .await
            .expect_err("an abandoned batch must not look like success");
        assert!(error.to_string().contains("acknowledgement"), "{error}",);
        abandoning.await.unwrap();
    }

    // --- row conversion ---

    #[test]
    fn a_row_without_an_external_id_is_rejected_rather_than_derived() {
        use get_pending_managed_wallet_creations::GetPendingManagedWalletCreationsResultData;

        let missing = GetPendingManagedWalletCreationsResultData { external_id: None };
        assert!(DeriveWalletRequest::try_from(missing).is_err());

        let present = GetPendingManagedWalletCreationsResultData {
            external_id: Some("wallet-1".to_string()),
        };
        assert_eq!(
            DeriveWalletRequest::try_from(present).unwrap().external_id,
            "wallet-1"
        );
    }
}
