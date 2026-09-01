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
            fetch_now = false;

            match self.get_pending_wallets(cursor.clone()).await {
                Ok((requests, next_cursor)) if requests.is_empty() => {
                    if next_cursor.is_some() {
                        // Keep draining when a raw page contained only
                        // malformed rows so later valid wallets are not
                        // starved behind it.
                        cursor = next_cursor;
                        restart_after_scan = true;
                        fetch_now = true;
                    } else {
                        cursor = None;
                        if fresh_lookup {
                            self.trigger.finish_empty_lookup();
                        } else {
                            fetch_now = self.trigger.finish_batch(true);
                        }
                        if restart_after_scan {
                            restart_after_scan = false;
                            let delay = crate::retry::jittered_exponential_delay(failure_count);
                            failure_count = failure_count.saturating_add(1);
                            if !fetch_now {
                                self.wait_for_retry_or_trigger(delay).await;
                            }
                            fetch_now = true;
                        } else {
                            failure_count = 0;
                        }
                        tracing::info!("No pending managed wallets");
                    }
                }
                Ok((requests, next_cursor)) => {
                    let scan_complete = next_cursor.is_none();
                    let outcome = self.send_batch_and_wait(requests).await?;
                    let deferred = self.trigger.finish_batch(scan_complete);

                    match outcome {
                        BatchOutcome::Completed => {
                            cursor = next_cursor;
                            if cursor.is_none() && restart_after_scan {
                                restart_after_scan = false;
                                let delay = crate::retry::jittered_exponential_delay(failure_count);
                                failure_count = failure_count.saturating_add(1);
                                if !deferred {
                                    self.wait_for_retry_or_trigger(delay).await;
                                }
                                fetch_now = true;
                            } else {
                                failure_count = 0;
                                fetch_now = cursor.is_some() || deferred;
                            }
                        }
                        BatchOutcome::SubmissionFailed => {
                            // Continue the current scan after the delay when
                            // possible. A poison row in one page must not hide
                            // valid wallets on every later page.
                            cursor = next_cursor;
                            restart_after_scan = cursor.is_some();
                            let delay = crate::retry::jittered_exponential_delay(failure_count);
                            failure_count = failure_count.saturating_add(1);
                            tracing::warn!(
                                "Managed-wallet batch submission failed; continuing lookup in {:.1}s",
                                delay.as_secs_f64(),
                            );
                            if !deferred {
                                self.wait_for_retry_or_trigger(delay).await;
                            }
                            fetch_now = true;
                        }
                    }
                }
                Err(error) => {
                    cursor = None;
                    restart_after_scan = false;
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

    async fn wait_for_retry_or_trigger(&self, delay: std::time::Duration) {
        tokio::select! {
            _ = sleep(delay) => {}
            _ = self.trigger.wait_until_ready() => {
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
                let derive_junction = match external_id.parse::<i64>() {
                    Ok(number) => DeriveJunction::soft(number),
                    Err(_) => DeriveJunction::soft(external_id.clone()),
                };

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
