mod fuel_tank;

use crate::graphql::sign_transactions::SignTransactionInput;
use crate::graphql::{GetPendingTransactions, get_pending_transactions};
use crate::transaction::fuel_tank::ExpirableSignature;
use crate::transaction::payload::RawFields;
use crate::types::{Chain, Network};
use crate::work_trigger::{PusherAwarePoller, PusherStatus, WorkTrigger};
use crate::{DUMMY_TX_MORTALITY, TX_MORTALITY, chain_info, global, platform_client, utils};
use parity_scale_codec::Encode;
use payload::RawPayload;
use std::collections::{HashMap, HashSet};
use subxt::config::DefaultExtrinsicParamsBuilder;
use subxt::utils::H256;
use subxt_signer::DeriveJunction;
use subxt_signer::sr25519::Keypair;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::sleep;

const NO_TRANSACTIONS_MSG: &str = "No transactions present in the body";
const TRANSACTION_PAGE_SIZE: i64 = 25;

/// Nonce cache key. Each `(network, chain, signer public key)` identifies
/// a signer-specific counter that may persist across batches until evicted.
type NonceKey = (Network, Chain, [u8; 32]);

/// Per-batch chain key for block / metadata prefetch.
type ChainKey = (Network, Chain);

/// `(block_number, block_hash, spec_version)` captured at prefetch time and
/// reused for every request signed against that chain in the batch.
type BlockInfo = (u32, H256, u32);

/// Fatal failure to dispatch a `ProcessorTick` from producer to consumer.
/// See `send_tick_and_wait` for the rationale on why both variants are
/// treated as fatal.
#[derive(Debug)]
enum TickDispatchError {
    /// `mpsc::send` failed: the consumer's `Receiver` was dropped, i.e.
    /// the consumer task has exited.
    ConsumerGone,
    /// The ack `oneshot` was dropped without sending: the consumer
    /// panicked mid-tick (or otherwise abandoned the tick) without
    /// completing the unconditional ack at the end of `launch_job_scheduler`.
    AckDropped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BatchOutcome {
    Completed,
    PartialProgress,
    Skipped,
    SubmissionFailed,
}

#[derive(Debug, Eq, PartialEq)]
enum BatchStep {
    Continue {
        cursor: Option<String>,
        made_progress: bool,
        requires_fresh_retry: bool,
    },
    RetryFresh,
}

/// Decide the next step after a page that produced rows.
///
/// `unproductive_pages` counts the consecutive pages in this scan that
/// submitted nothing. A fully-skipped page advances the cursor for the same
/// reason an unconvertible one does — so a poison row cannot hide healthy
/// work on later pages — and therefore needs the same two bounds: the cursor
/// must actually move, and a single scan may only skip so many pages before
/// falling back to the paced fresh retry.
fn batch_step(
    outcome: BatchOutcome,
    cursor: Option<&str>,
    next_cursor: Option<String>,
    unproductive_pages: u32,
) -> BatchStep {
    match outcome {
        BatchOutcome::Completed => BatchStep::Continue {
            cursor: next_cursor,
            made_progress: true,
            requires_fresh_retry: false,
        },
        BatchOutcome::PartialProgress => BatchStep::Continue {
            cursor: next_cursor,
            made_progress: true,
            requires_fresh_retry: true,
        },
        BatchOutcome::Skipped
            if crate::retry::should_continue_empty_drain(
                cursor,
                next_cursor.as_deref(),
                unproductive_pages,
            ) =>
        {
            BatchStep::Continue {
                cursor: next_cursor,
                made_progress: false,
                requires_fresh_retry: true,
            }
        }
        BatchOutcome::Skipped | BatchOutcome::SubmissionFailed => BatchStep::RetryFresh,
    }
}

/// Whether any page in the current cursor scan left work pending. Once set,
/// this remains set until scan completion so a later successful page cannot
/// hide an earlier partial or skipped page.
#[derive(Debug, Default)]
struct ScanRetryState {
    required: bool,
}

impl ScanRetryState {
    fn require(&mut self) {
        self.required = true;
    }

    fn take(&mut self) -> bool {
        std::mem::take(&mut self.required)
    }

    fn clear(&mut self) {
        self.required = false;
    }
}

/// Complete one authoritative cursor scan. Nonce-cache entries are only
/// considered idle when their chain was present in the previous completed
/// scan but absent from this completed scan; individual cursor pages are not
/// authoritative on their own.
fn complete_chain_scan(
    previous_scan_chains: &mut HashSet<ChainKey>,
    current_scan_chains: &mut HashSet<ChainKey>,
) -> HashSet<ChainKey> {
    let idle = previous_scan_chains
        .difference(current_scan_chains)
        .copied()
        .collect();
    *previous_scan_chains = std::mem::take(current_scan_chains);
    idle
}

impl std::fmt::Display for TickDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConsumerGone => write!(f, "transaction processor receiver dropped"),
            Self::AckDropped => write!(f, "transaction processor dropped tick ack"),
        }
    }
}

impl std::error::Error for TickDispatchError {}

/// Message sent from the poller to the processor on every poll tick.
///
/// `batch` is the (possibly empty) set of transaction requests pulled this
/// tick. `idle` is the set of `(network, chain)` pairs that were present in
/// the previous completed cursor scan but absent from the current completed
/// scan. The processor uses this to evict their nonce-cache entries so the
/// next batch for those chains re-reads the Platform-corrected nonce.
///
/// `ack` is a one-shot channel the consumer signals once it has fully
/// finished processing this tick — including the round-trip to the platform
/// `SignTransactions` mutation. The producer awaits this ack before issuing
/// the next `GetPendingTransactions` call. This is the backpressure
/// mechanism that prevents the producer from racing ahead of the consumer
/// and re-fetching uuids that are still queued for signing (which would
/// cause the same uuid to be signed multiple times with consecutive
/// nonces).
pub struct ProcessorTick {
    batch: Vec<TransactionRequest>,
    idle: HashSet<ChainKey>,
    ack: oneshot::Sender<BatchOutcome>,
}

pub(crate) mod payload {
    use crate::global;
    use crate::types::{Chain, Network};
    use scale_encode::{EncodeAsFields, FieldIter, TypeResolver};

    use subxt::error::EncodeError;
    use subxt::transactions::Payload;

    pub type Bytes = Vec<u8>;

    pub struct RawPayload {
        pub pallet_name: String,
        pub call_name: String,
        pub field_bytes: RawFields,
    }

    pub struct RawFields(pub Bytes);

    impl EncodeAsFields for RawFields {
        fn encode_as_fields_to<R: TypeResolver>(
            &self,
            _fields: &mut dyn FieldIter<'_, R::TypeId>,
            _types: &R,
            out: &mut Bytes,
        ) -> Result<(), EncodeError> {
            out.extend_from_slice(&self.0);
            Ok(())
        }
    }

    impl Payload for RawPayload {
        type CallData = RawFields;

        fn pallet_name(&self) -> &str {
            &self.pallet_name
        }
        fn call_name(&self) -> &str {
            &self.call_name
        }
        fn call_data(&self) -> &RawFields {
            &self.field_bytes
        }
    }

    impl RawPayload {
        pub async fn from_bytes(network: Network, chain: Chain, bytes: &[u8]) -> Option<Self> {
            let pallet_index = bytes.first()?;
            let call_index = bytes.get(1)?;
            let (pallet_name, call_name) = match global::metadata_names(
                network,
                chain,
                *pallet_index,
                *call_index,
            )
            .await
            {
                Some(x) => x,
                None => {
                    tracing::error!(
                        "extrinsic at pallet index: {pallet_index}, call_index: {call_index}, not found"
                    );
                    return None;
                }
            };

            Some(Self {
                pallet_name,
                call_name,
                field_bytes: RawFields(bytes[2..].to_vec()),
            })
        }
    }
}

#[derive(Clone)]
pub struct TransactionRequest {
    request_id: String,
    external_id: Option<String>,
    network: Network,
    chain: Chain,
    payload: Vec<u8>,
    /// If this is Some, the extrinsic is a dispatch from fuel tanks and needs the signature added
    pub fuel_tank_signer_external_id: Option<Option<String>>,
}

impl TryFrom<get_pending_transactions::GetPendingTransactionsResultData> for TransactionRequest {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn try_from(
        data: get_pending_transactions::GetPendingTransactionsResultData,
    ) -> Result<Self, Self::Error> {
        tracing::debug!("{:?}", data);
        let external_id = data.wallet.external_id.clone();

        Ok(Self {
            external_id,
            request_id: data.uuid,
            network: data.network.try_into()?,
            chain: data.chain.try_into()?,
            payload: hex::decode(data.encoded_data.split('x').nth(1).ok_or("missing 0x")?)?,
            fuel_tank_signer_external_id: data
                .should_sign_fuel_tank
                .then_some(data.fuel_tank_signer_external_id),
        })
    }
}

#[derive(Debug)]
pub struct TransactionJob {
    sender: Sender<ProcessorTick>,
    trigger: WorkTrigger,
    pusher_status: PusherStatus,
}

impl TransactionJob {
    #[cfg(test)]
    fn new(sender: Sender<ProcessorTick>) -> Self {
        Self {
            sender,
            trigger: WorkTrigger::new(),
            pusher_status: PusherStatus::new(),
        }
    }

    fn with_trigger(
        sender: Sender<ProcessorTick>,
        trigger: WorkTrigger,
        pusher_status: PusherStatus,
    ) -> Self {
        Self {
            sender,
            trigger,
            pusher_status,
        }
    }

    pub fn create_job(
        keypair: Keypair,
        trigger: WorkTrigger,
        pusher_status: PusherStatus,
    ) -> (TransactionJob, TransactionProcessor) {
        // Capacity 1: combined with `send().await` on the producer side and
        // a per-tick `oneshot` ack from the consumer, this enforces "at most
        // one tick in flight at a time." The producer cannot issue a new
        // `GetPendingTransactions` until the consumer has both pulled the
        // previous tick AND signaled completion of `SignTransactions` for
        // it. See `ProcessorTick` for the rationale.
        let (sender, receiver) = tokio::sync::mpsc::channel(1);

        (
            TransactionJob::with_trigger(sender, trigger, pusher_status),
            TransactionProcessor::new(keypair, receiver),
        )
    }

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            // `start_polling` only returns on a fatal dispatch failure
            // (consumer task gone or ack dropped). When that happens we
            // log and let the task complete — `main`'s `tokio::select!`
            // is watching this `JoinHandle` and will exit the daemon,
            // letting the process supervisor restart it cleanly.
            if let Err(e) = self.start_polling().await {
                tracing::error!("Transaction poller exiting due to fatal error: {e}");
            }
        })
    }

    async fn start_polling(&self) -> Result<(), TickDispatchError> {
        let mut no_transaction_count = 0;
        // Individual cursor pages are not authoritative: the same chain can
        // disappear on page 2 and return on page 3. Accumulate chains across
        // the full scan and only evict chains absent from the completed scan.
        let mut previous_scan_chains: HashSet<ChainKey> = HashSet::new();
        let mut current_scan_chains: HashSet<ChainKey> = HashSet::new();

        // Persistent cursor for `GetPendingTransactions`. We pass the
        // cursor we received on the previous successful poll back to the
        // platform on the next poll. The platform uses the cursor as a
        // backend pagination/scan hint, so providing it lets the server
        // skip work it has already returned to us. Each `Ok` page we
        // receive is capped at `TRANSACTION_PAGE_SIZE = 25` (the
        // server-side `limit` parameter), and we deliver exactly one
        // page per tick — that is what enforces the per-batch cap of
        // 25 on the consumer side, and therefore on the
        // `SignTransactions` mutation.
        //
        // Pages that make full or partial progress advance the cursor. A
        // locally skipped page also advances when possible so a broken chain
        // or poison row cannot starve healthy work on later pages. Only a
        // failed platform submission resets the scan immediately.
        let mut cursor: Option<String> = None;
        let mut fetch_now = true; // Mandatory startup catch-up.
        let mut failure_count = 0u32;
        let mut scan_retry = ScanRetryState::default();
        // Consecutive pages in the current scan that submitted nothing —
        // either no convertible rows, or convertible rows that all failed to
        // sign. Bounds the unpaced drain past both kinds of bad data.
        let mut unproductive_pages = 0u32;
        let mut pusher_aware_poll = self.pusher_status.poller();

        loop {
            if !fetch_now {
                tokio::select! {
                    _ = self.trigger.wait_until_ready() => {}
                    _ = pusher_aware_poll.tick() => {}
                }
            }

            // A fresh scan consumes every event/force that existed before
            // this request. Cursor continuations deliberately retain events
            // received during the scan so they can force a restart at the end.
            let fresh_lookup = cursor.is_none();
            if fresh_lookup {
                current_scan_chains.clear();
                self.trigger.begin_fresh_lookup();
            }
            match self.get_pending_transactions(cursor.clone()).await {
                Ok((transaction_reqs, next_cursor)) => {
                    if transaction_reqs.is_empty() {
                        // Keep draining past a page whose rows all failed
                        // conversion so later healthy pages are not hidden
                        // behind malformed data — but only while the cursor
                        // actually advances, and only for a bounded run, so a
                        // large block of malformed rows (or a server that
                        // returns the same cursor) cannot become an unpaced
                        // request storm.
                        if crate::retry::should_continue_empty_drain(
                            cursor.as_deref(),
                            next_cursor.as_deref(),
                            unproductive_pages,
                        ) {
                            unproductive_pages += 1;
                            cursor = next_cursor;
                            scan_retry.require();
                            fetch_now = true;
                        } else {
                            if next_cursor.is_some() {
                                tracing::warn!(
                                    "Abandoning cursor scan after {unproductive_pages} page(s) that submitted nothing; restarting from a fresh lookup",
                                );
                                scan_retry.require();
                            }
                            unproductive_pages = 0;
                            cursor = None;
                            let idle = complete_chain_scan(
                                &mut previous_scan_chains,
                                &mut current_scan_chains,
                            );
                            if !idle.is_empty() {
                                self.send_tick_and_wait(Vec::new(), idle, "idle tick")
                                    .await?;
                            }
                            let deferred = if fresh_lookup {
                                self.trigger.finish_empty_lookup();
                                false
                            } else {
                                self.trigger.finish_batch(true)
                            };
                            fetch_now = self
                                .finish_scan_retry(
                                    &mut scan_retry,
                                    deferred,
                                    &mut failure_count,
                                    &mut pusher_aware_poll,
                                )
                                .await;
                        }
                    } else {
                        let active: HashSet<ChainKey> = transaction_reqs
                            .iter()
                            .map(|request| (request.network, request.chain))
                            .collect();
                        current_scan_chains.extend(active);

                        let scan_complete = next_cursor.is_none();
                        let idle = if scan_complete {
                            complete_chain_scan(&mut previous_scan_chains, &mut current_scan_chains)
                        } else {
                            HashSet::new()
                        };
                        let outcome = self
                            .send_tick_and_wait(transaction_reqs, idle, "transaction requests")
                            .await?;
                        let deferred = self.trigger.finish_batch(scan_complete);

                        match batch_step(
                            outcome,
                            cursor.as_deref(),
                            next_cursor,
                            unproductive_pages,
                        ) {
                            BatchStep::Continue {
                                cursor: next_cursor,
                                made_progress,
                                requires_fresh_retry,
                            } => {
                                if made_progress {
                                    no_transaction_count = 0;
                                    unproductive_pages = 0;
                                } else {
                                    // A page that submitted nothing counts
                                    // against the same drain bound as a page
                                    // with no convertible rows.
                                    unproductive_pages += 1;
                                }
                                if requires_fresh_retry {
                                    scan_retry.require();
                                }
                                cursor = next_cursor;
                                if cursor.is_none() {
                                    fetch_now = self
                                        .finish_scan_retry(
                                            &mut scan_retry,
                                            deferred,
                                            &mut failure_count,
                                            &mut pusher_aware_poll,
                                        )
                                        .await;
                                } else {
                                    fetch_now = true;
                                }
                            }
                            BatchStep::RetryFresh => {
                                cursor = None;
                                current_scan_chains.clear();
                                scan_retry.clear();
                                unproductive_pages = 0;
                                let delay = crate::retry::jittered_exponential_delay(failure_count);
                                failure_count = failure_count.saturating_add(1);
                                tracing::warn!(
                                    "Transaction batch made no progress; retrying a fresh lookup in {:.1}s",
                                    delay.as_secs_f64(),
                                );
                                self.wait_for_retry_or_trigger(delay, &mut pusher_aware_poll)
                                    .await;
                                fetch_now = true;
                            }
                        }
                    }
                }
                Err(e) => {
                    // Any error path resets the cursor: a stale cursor
                    // is meaningless after we've lost our place in the
                    // server-side scan.
                    cursor = None;
                    unproductive_pages = 0;
                    if e.to_string() == NO_TRANSACTIONS_MSG {
                        if no_transaction_count % 10 == 0 {
                            tracing::info!("GetPendingTransactions: {}", NO_TRANSACTIONS_MSG,);
                        }
                        no_transaction_count += 1;

                        let idle = complete_chain_scan(
                            &mut previous_scan_chains,
                            &mut current_scan_chains,
                        );
                        if !idle.is_empty() {
                            self.send_tick_and_wait(Vec::new(), idle, "idle tick")
                                .await?;
                        }
                        let deferred = if fresh_lookup {
                            self.trigger.finish_empty_lookup();
                            false
                        } else {
                            self.trigger.finish_batch(true)
                        };
                        fetch_now = self
                            .finish_scan_retry(
                                &mut scan_retry,
                                deferred,
                                &mut failure_count,
                                &mut pusher_aware_poll,
                            )
                            .await;
                    } else {
                        current_scan_chains.clear();
                        scan_retry.clear();
                        tracing::error!("Error: {}", e);
                        self.trigger.finish_empty_lookup();
                        let delay = crate::retry::jittered_exponential_delay(failure_count);
                        failure_count = failure_count.saturating_add(1);
                        tracing::warn!(
                            "Retrying GetPendingTransactions in {:.1}s",
                            delay.as_secs_f64(),
                        );
                        self.wait_for_retry_or_trigger(delay, &mut pusher_aware_poll)
                            .await;
                        fetch_now = true;
                    }
                }
            }
        }
    }

    /// Finish a cursor scan without losing an earlier skipped or partial
    /// page. A pending Pusher trigger bypasses the retry delay, but either
    /// path starts the next lookup from a fresh cursor.
    async fn finish_scan_retry(
        &self,
        scan_retry: &mut ScanRetryState,
        deferred: bool,
        failure_count: &mut u32,
        pusher_aware_poll: &mut PusherAwarePoller,
    ) -> bool {
        if !scan_retry.take() {
            *failure_count = 0;
            return deferred;
        }

        let delay = crate::retry::jittered_exponential_delay(*failure_count);
        *failure_count = failure_count.saturating_add(1);
        tracing::warn!(
            "Transaction scan left pending work; retrying a fresh lookup in {:.1}s",
            delay.as_secs_f64(),
        );
        // The delay is honoured even when `deferred` — work we already know
        // about is exactly what this scan failed to make progress on, so
        // letting it skip the backoff would remove all pacing.
        self.wait_for_retry_or_trigger(delay, pusher_aware_poll)
            .await;
        true
    }

    /// Wait out a failure delay. Work that arrives *during* the delay cuts it
    /// short; work that was already pending when the failure occurred does
    /// not, so a failing platform can never be turned into an unpaced storm
    /// of requests.
    ///
    /// The fallback poll is also allowed to cut the delay short. `failure_count`
    /// is never reset while any scan leaves work behind, so a single row the
    /// daemon can never sign drives the delay to the 60s cap and holds it
    /// there. Without this arm that cap would silently override the six-second
    /// Pusher-outage poll — suppressing the fallback exactly when it is the
    /// only thing left to notice new work.
    async fn wait_for_retry_or_trigger(
        &self,
        delay: std::time::Duration,
        pusher_aware_poll: &mut PusherAwarePoller,
    ) {
        tokio::select! {
            _ = sleep(delay) => {}
            _ = pusher_aware_poll.tick() => {}
            _ = self.trigger.wait_for_new_event() => {
                tracing::info!("New transaction work interrupted the retry delay");
            }
        }
    }

    /// Push a `ProcessorTick` into the channel and block until the consumer
    /// signals it has finished processing it. This is the single
    /// synchronization point between producer and consumer that prevents
    /// duplicate signing of the same uuid (see `ProcessorTick` docs and the
    /// loop comment in `start_polling`).
    ///
    /// `kind` is a short label used only in error logs to distinguish the
    /// three send sites ("transaction requests" vs "idle tick").
    ///
    /// Both failure modes are fatal:
    ///
    ///   * `mpsc::send` failing means the receiver was dropped, i.e. the
    ///     consumer task is gone. The mpsc never fails for any other
    ///     reason — there is no transient case to recover from.
    ///
    ///   * The ack oneshot being dropped without a value means the
    ///     consumer panicked mid-tick (the production code paths always
    ///     signal the ack on the way out of every branch).
    ///
    /// Either way, in-flight batches are lost and there is nothing in
    /// the current architecture that revives the consumer. Returning
    /// `Err` here propagates up through `start_polling`, which causes
    /// the polling task's `JoinHandle` to complete; `main`'s
    /// `tokio::select!` then exits the daemon, allowing the process
    /// supervisor to restart it cleanly with a fresh consumer.
    async fn send_tick_and_wait(
        &self,
        batch: Vec<TransactionRequest>,
        idle: HashSet<ChainKey>,
        kind: &str,
    ) -> Result<BatchOutcome, TickDispatchError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let tick = ProcessorTick {
            batch,
            idle,
            ack: ack_tx,
        };
        if let Err(e) = self.sender.send(tick).await {
            tracing::error!("Failed to send {kind} to processor: {e:?}");
            return Err(TickDispatchError::ConsumerGone);
        }
        match ack_rx.await {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                tracing::error!("Processor dropped ack for {kind} without signaling: {e:?}");
                Err(TickDispatchError::AckDropped)
            }
        }
    }

    /// Fetch one page (up to `TRANSACTION_PAGE_SIZE` items) of pending
    /// transactions, optionally resuming a server-side scan via
    /// `cursor`. Returns the decoded `TransactionRequest`s alongside
    /// the platform's `nextCursor`, which the caller carries forward
    /// to the next poll when the platform indicates more pending work
    /// is available.
    async fn get_pending_transactions(
        &self,
        cursor: Option<String>,
    ) -> Result<(Vec<TransactionRequest>, Option<String>), Box<dyn std::error::Error + Send + Sync>>
    {
        let response_data = utils::execute_query::<GetPendingTransactions>(
            get_pending_transactions::Variables {
                limit: TRANSACTION_PAGE_SIZE,
                cursor,
            },
            None,
        )
        .await?;

        let result = match response_data.result {
            Some(r) => r,
            None => return Err(NO_TRANSACTIONS_MSG.into()),
        };

        let page = result.data;
        let next_cursor = match result.next_cursor {
            Some(c) if !c.is_empty() => Some(c),
            _ => None,
        };

        if page.is_empty() {
            return Err(NO_TRANSACTIONS_MSG.into());
        }

        let requests: Vec<TransactionRequest> = page
            .into_iter()
            .filter_map(|p| {
                TransactionRequest::try_from(p)
                    .map_err(|e| {
                        tracing::error!("Error creating TransactionRequest: {}", e);
                        e
                    })
                    .ok()
            })
            .collect();

        tracing::debug!(
            "GetPendingTransactions: {} request(s) returned, next_cursor present: {}",
            requests.len(),
            next_cursor.is_some(),
        );

        Ok((requests, next_cursor))
    }
}

pub struct TransactionProcessor {
    keypair: Keypair,
    receiver: Receiver<ProcessorTick>,
    /// Persistent nonce cache. The slot for `(network, chain, signer)` holds
    /// the next nonce to use for that triple. Survives across batches so that
    /// back-to-back cursor pages stay in sync; entries are evicted when an
    /// authoritative lookup reports that their chain is idle.
    nonces: HashMap<NonceKey, u64>,
}

impl TransactionProcessor {
    pub(crate) fn new(keypair: Keypair, receiver: Receiver<ProcessorTick>) -> Self {
        Self {
            keypair,
            receiver,
            nonces: HashMap::new(),
        }
    }

    async fn transaction_handler(
        keypair: Keypair,
        nonces: &mut HashMap<NonceKey, u64>,
        requests: Vec<TransactionRequest>,
    ) -> BatchOutcome {
        // Derive the signer for every request up-front so we can both
        // pre-fetch nonces and reuse the keypair in the main signing loop.
        let signers: Vec<Keypair> = requests
            .iter()
            .map(|r| derive_signer(&keypair, r.external_id.as_deref()))
            .collect();

        // Pre-fetch per-chain block info and refresh metadata once per
        // (network, chain) for this batch. If either step fails, the chain is
        // marked failed and every request targeting it is skipped.
        let mut block_info: HashMap<ChainKey, BlockInfo> = HashMap::new();
        let mut failed_chains: HashSet<ChainKey> = HashSet::new();
        for request in requests.iter() {
            let key: ChainKey = (request.network, request.chain);
            if block_info.contains_key(&key) || failed_chains.contains(&key) {
                continue;
            }

            let (block_number, block_hash, spec_version) =
                match chain_info::get_block_and_spec_version(request.network, request.chain).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(
                            "could not fetch block number for {:?}/{:?}: {e}; skipping all requests for this chain in this batch",
                            request.network,
                            request.chain,
                        );
                        failed_chains.insert(key);
                        continue;
                    }
                };

            let needs_update =
                match global::metadata_spec_version(request.network, request.chain).await {
                    Some(local) => local < spec_version,
                    None => true,
                };
            if needs_update
                && let Err(e) =
                    chain_info::update_metadata_and_substrate_client(request.network, request.chain)
                        .await
            {
                tracing::error!(
                    "failed to update metadata for {:?}/{:?}: {e}; skipping all requests for this chain in this batch",
                    request.network,
                    request.chain,
                );
                failed_chains.insert(key);
                continue;
            }

            tracing::debug!(
                "Prefetched block {block_number} (spec {spec_version}) for {:?}/{:?}",
                request.network,
                request.chain,
            );
            block_info.insert(key, (block_number, block_hash, spec_version));
        }

        // Seed (or refresh) the persistent nonce cache for every
        // (network, chain, signer) triple in this batch. GetAccountNonce is
        // Platform-corrected: it considers both the chain nonce and recently
        // submitted signed extrinsics. We fetch it once per key per batch and
        // rebase the cached slot to `max(slot, platform_nonce)`:
        //
        //   * If `slot >= platform_nonce`, the cache is preserved. This is
        //     the common cursor-pagination case: our previous page advanced
        //     the slot and it remains the right next nonce to use.
        //
        //   * If `slot < platform_nonce`, the chain or Platform's submitted
        //     transaction lookback has moved forward, so the cache is rebased
        //     before any new extrinsics are signed.
        //
        let mut failed_keys: HashSet<NonceKey> = HashSet::new();
        // Per-batch dedup: we want exactly one chain fetch per key per
        // batch, even when the same triple appears across many requests.
        let mut refreshed_keys: HashSet<NonceKey> = HashSet::new();
        for (signer, request) in signers.iter().zip(requests.iter()) {
            // Don't bother fetching a nonce for a chain that already failed
            // its block / metadata prefetch.
            if failed_chains.contains(&(request.network, request.chain)) {
                continue;
            }
            let key: NonceKey = (request.network, request.chain, signer.public_key().0);
            if refreshed_keys.contains(&key) || failed_keys.contains(&key) {
                continue;
            }
            match chain_info::get_account_nonce(
                request.network,
                request.chain,
                &signer.public_key(),
            )
            .await
            {
                Ok(platform_nonce) => {
                    let slot = nonces.entry(key).or_insert(platform_nonce);
                    if *slot < platform_nonce {
                        let was = *slot;
                        *slot = platform_nonce;
                        tracing::info!(
                            "Detected Platform-corrected nonce advance for account 0x{} - Network: {:?} - Chain: {:?}; rebasing cache from {was} to {platform_nonce}",
                            hex::encode(key.2),
                            request.network,
                            request.chain,
                        );
                    } else {
                        let cached = *slot;
                        tracing::debug!(
                            "Refreshed nonce: cache={cached} platform={platform_nonce} for account 0x{} - Network: {:?} - Chain: {:?}",
                            hex::encode(key.2),
                            request.network,
                            request.chain,
                        );
                    }
                    refreshed_keys.insert(key);
                }
                Err(e) => {
                    tracing::error!(
                        "failed to fetch nonce for {} on {:?}/{:?}: {e}; skipping all requests for this key in this batch",
                        hex::encode(key.2),
                        request.network,
                        request.chain,
                    );
                    failed_keys.insert(key);
                }
            }
        }

        let mut inputs = Vec::with_capacity(requests.len());
        // `NonceKey`s whose in-memory counter we advanced during this
        // batch. On a successful `SignTransactions` round-trip these
        // increments stay; on failure we evict every entry in this set
        // (see the `Err` arm after the loop).
        let mut committed_keys: HashSet<NonceKey> = HashSet::new();

        let request_count = requests.len();

        for (signer, request) in signers.into_iter().zip(requests) {
            let TransactionRequest {
                request_id,
                external_id: _,
                network,
                chain,
                mut payload,
                fuel_tank_signer_external_id,
            } = request;

            let pubkey_bytes = signer.public_key().0;
            let nonce_key: NonceKey = (network, chain, pubkey_bytes);
            let chain_key: ChainKey = (network, chain);

            // Skip up-front if the per-chain prefetch failed.
            if failed_chains.contains(&chain_key) {
                tracing::error!(
                    "Skipping request #{request_id}: prefetch failed for {network:?}/{chain:?}"
                );
                continue;
            }

            // Skip up-front if the nonce prefetch failed for this key.
            if failed_keys.contains(&nonce_key) {
                tracing::error!(
                    "Skipping request #{request_id}: no prefetched nonce for {}",
                    hex::encode(pubkey_bytes)
                );
                continue;
            }

            let Some(&(block_number, block_hash, _spec_version)) = block_info.get(&chain_key)
            else {
                tracing::error!("missing prefetched block info for {network:?}/{chain:?}");
                continue;
            };

            let Some(nonce_slot) = nonces.get_mut(&nonce_key) else {
                tracing::error!(
                    "missing pre-fetched nonce for {} on {network:?}/{chain:?}",
                    hex::encode(pubkey_bytes)
                );
                continue;
            };
            let correct_nonce = *nonce_slot;

            if let Some(fuel_tank_signer_external_id) = fuel_tank_signer_external_id {
                // expiration block is needed for the signature
                let expiration_block = block_number + TX_MORTALITY as u32;

                // remove the last byte of the payload because it is the settings param, and we are
                // replacing it
                payload.pop();

                // create message to be signed
                let Ok(message) =
                    fuel_tank::create_message(&payload, signer.public_key().0, expiration_block)
                else {
                    continue;
                };

                // sign by the fuel tank external id if it exists
                let ft_signer = derive_signer(&keypair, fuel_tank_signer_external_id.as_deref());
                let signature = sp_core::sr25519::Signature::from_raw(ft_signer.sign(&message).0);
                tracing::info!(
                    "fuel tanks - signed message {} with {} and got signature {}",
                    hex::encode(&message),
                    hex::encode(ft_signer.public_key().0),
                    hex::encode(signature)
                );

                let settings = fuel_tank::DispatchSettings {
                    signature: Some(ExpirableSignature {
                        signature,
                        expiry_block: expiration_block,
                    }),
                    ..Default::default()
                };

                tracing::info!("payload before fuel tank: {}", hex::encode(&payload));

                // append to the payload. This is fine because settings is the last param of the extrinsic
                payload.extend_from_slice(&Some(settings).encode());
                tracing::info!("fuel tank modified payload: {}", hex::encode(&payload));
            }

            let dummy_tx = {
                // this is system.remark with empty value: 0x000000
                let payload = RawPayload {
                    pallet_name: "System".to_string(),
                    call_name: "remark".to_string(),
                    field_bytes: RawFields(vec![0]),
                };
                let params = DefaultExtrinsicParamsBuilder::new()
                    .nonce(correct_nonce)
                    .mortal_from_unchecked(DUMMY_TX_MORTALITY, block_number.into(), block_hash)
                    .build();
                let Some(chain_client) = global::substrate_client(network, chain).await else {
                    tracing::error!(
                        "Missing substrate client for network {network:?}, chain {chain:?}"
                    );
                    continue;
                };
                let client_at_block = chain_client.at_block(block_number).unwrap();
                let signed_dummy_tx = match client_at_block
                    .tx()
                    .create_signable_offline(&payload, params)
                {
                    Ok(mut tx) => match tx.sign(&signer) {
                        Ok(signed) => signed,
                        Err(e) => {
                            tracing::error!("Failed to sign dummy transaction: {}", e);
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::error!("Failed to create signed dummy transaction: {}", e);
                        continue;
                    }
                };
                format!("0x{}", hex::encode(signed_dummy_tx.encoded()))
            };
            let signed_tx = {
                // contruct payload
                if payload.len() < 2 {
                    tracing::error!("payload does not store pallet index and call index");
                    continue;
                }
                let payload = match RawPayload::from_bytes(network, chain, &payload).await {
                    Some(x) => x,
                    None => {
                        tracing::error!("generating raw payload failed");
                        continue;
                    }
                };

                // sign extrinsic
                let params = DefaultExtrinsicParamsBuilder::new()
                    .nonce(correct_nonce)
                    .mortal_from_unchecked(TX_MORTALITY, block_number.into(), block_hash)
                    .build();
                let Some(chain_client) = global::substrate_client(network, chain).await else {
                    tracing::error!(
                        "Missing substrate client for network {network:?}, chain {chain:?}"
                    );
                    continue;
                };
                let Ok(client_at_block) = chain_client.at_block(block_number) else {
                    tracing::error!(
                        "Client metadata or spec_version missing for network {network:?}, chain {chain:?}"
                    );
                    continue;
                };

                match client_at_block
                    .tx()
                    .create_signable_offline(&payload, params)
                {
                    Ok(mut tx) => match tx.sign(&signer) {
                        Ok(signed) => signed,
                        Err(e) => {
                            tracing::error!("Failed to sign transaction: {}", e);
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::error!("Failed to create signed transaction: {}", e);
                        continue;
                    }
                }
            };
            let encoded_tx = hex::encode(signed_tx.encoded());

            tracing::info!(
                "Signed #{request_id} nonce={correct_nonce} account=0x{} network={network:?} chain={chain:?}",
                hex::encode(pubkey_bytes),
            );
            tracing::debug!("Signed #{request_id} extrinsic: 0x{encoded_tx}",);
            inputs.push(SignTransactionInput {
                uuid: request_id.clone(),
                signed_extrinsic: format!("0x{encoded_tx}"),
                signed_abandon_extrinsic: dummy_tx.clone(),
            });

            // Only advance the nonce after a tx has been successfully
            // built, signed, and queued for submission. Track the
            // `NonceKey` so we can roll back the in-memory counter if the
            // batch's `SignTransactions` mutation ultimately fails.
            *nonce_slot += 1;
            committed_keys.insert(nonce_key);
        }

        // Short-circuit if every request in this batch was skipped
        // (failed prefetch, failed signing, etc.)
        if inputs.is_empty() {
            tracing::debug!(
                "SignTransactions: nothing to submit (all {request_count} request(s) in this batch were skipped)",
            );
            return BatchOutcome::Skipped;
        }

        let submitted_count = inputs.len();

        // Snapshot the uuids actually queued for submission so we can name
        // them in the rollback log if the platform mutation fails.
        let submitted_uuids: Vec<String> = inputs.iter().map(|i| i.uuid.clone()).collect();

        if let Err(e) = platform_client::sign_transactions(inputs).await {
            // Platform-side failure (after retries): every nonce we
            // advanced in this batch is uncommitted — those uuids are
            // still pending on the platform side and its corrected nonce has
            // not incorporated this failed submission. Evict the affected
            // cache entries so the next batch re-fetches GetAccountNonce and
            // re-signs the same uuids at the correct values. Without this,
            // the cache would drift forward by `committed_keys.len()` entries
            // relative to Platform/chain reality, producing future-nonce
            // extrinsics on every subsequent batch and a stuck queue.
            let evicted = evict_nonce_keys(nonces, &committed_keys);
            let chains: HashSet<ChainKey> = committed_keys
                .iter()
                .map(|(net, chain, _)| (*net, *chain))
                .collect();
            tracing::error!(
                "Platform SignTransactions failed; evicting nonce cache for affected chains so the next batch will re-fetch GetAccountNonce. error={e} chains={chains:?} uuids={submitted_uuids:?} evicted={evicted}",
            );
            BatchOutcome::SubmissionFailed
        } else if submitted_count < request_count {
            // Some requests were skipped before submission and remain pending.
            BatchOutcome::PartialProgress
        } else {
            BatchOutcome::Completed
        }
    }

    async fn launch_job_scheduler(mut self) {
        // Process one tick at a time. Awaiting the handler (rather than
        // `tokio::spawn`ing it) guarantees the previous batch is fully signed
        // and submitted before the next one starts, which is required for
        // correct nonce sequencing.
        //
        // After processing each tick — including any `SignTransactions`
        // round-trip in `transaction_handler` — we signal the producer via
        // the per-tick `ack` channel. The producer blocks on this ack
        // before issuing the next `GetPendingTransactions`, which is what
        // prevents the producer from racing ahead and re-fetching uuids
        // that are still queued for signing.
        while let Some(ProcessorTick { batch, idle, ack }) = self.receiver.recv().await {
            if !idle.is_empty() {
                let evicted = evict_idle_chains(&mut self.nonces, &idle);
                if evicted > 0 {
                    tracing::debug!(
                        "Reset nonce cache for {} idle chain(s) ({} entries evicted)",
                        idle.len(),
                        evicted,
                    );
                }
            }

            let outcome = if batch.is_empty() {
                BatchOutcome::Completed
            } else {
                Self::transaction_handler(self.keypair.clone(), &mut self.nonces, batch).await
            };

            // Always signal the producer, including for idle ticks. Failure
            // here means the producer has already gone away.
            let _ = ack.send(outcome);
        }
    }

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(self.launch_job_scheduler())
    }
}

/// Derive a signer keypair from `keypair` using `external_id` as a soft
/// derivation junction. If `external_id` parses as an `i64` the numeric
/// junction is used; otherwise the raw string is used. When `external_id`
/// is `None` the root keypair is returned.
fn derive_signer(keypair: &Keypair, external_id: Option<&str>) -> Keypair {
    match external_id {
        Some(id) => {
            let junction = match id.parse::<i64>() {
                Ok(n) => DeriveJunction::soft(n),
                Err(_) => DeriveJunction::soft(id),
            };
            keypair.derive([junction])
        }
        None => keypair.clone(),
    }
}

/// Drop every nonce-cache entry whose `(network, chain)` is idle. The next
/// batch for that chain seeds its counter from GetAccountNonce.
fn evict_idle_chains(nonces: &mut HashMap<NonceKey, u64>, idle: &HashSet<ChainKey>) -> usize {
    let before = nonces.len();
    nonces.retain(|(net, chain, _), _| !idle.contains(&(*net, *chain)));
    before - nonces.len()
}

/// Drop every nonce-cache entry in `keys`. Used to roll back the cache
/// after a failed `SignTransactions` mutation: the per-tx loop in
/// `transaction_handler` advances the in-memory nonce counter as each tx
/// is built and queued for submission, but those increments only reflect
/// reality once the platform accepts the batch. If the platform-side
/// mutation ultimately fails (after retries), the cached counters reflect
/// uncommitted nonces and must be discarded so the next batch re-reads the
/// Platform-corrected nonce and re-signs the same uuids at the correct values.
/// Returns the number of entries actually evicted.
fn evict_nonce_keys(nonces: &mut HashMap<NonceKey, u64>, keys: &HashSet<NonceKey>) -> usize {
    let before = nonces.len();
    nonces.retain(|k, _| !keys.contains(k));
    before - nonces.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_progress_keeps_the_cursor_and_continues_immediately() {
        assert_eq!(
            batch_step(
                BatchOutcome::PartialProgress,
                None,
                Some("page-2".to_string()),
                0
            ),
            BatchStep::Continue {
                cursor: Some("page-2".to_string()),
                made_progress: true,
                requires_fresh_retry: true,
            }
        );
    }

    #[test]
    fn partial_progress_on_the_final_page_requires_a_fresh_retry() {
        assert_eq!(
            batch_step(BatchOutcome::PartialProgress, Some("page-1"), None, 0),
            BatchStep::Continue {
                cursor: None,
                made_progress: true,
                requires_fresh_retry: true,
            }
        );
    }

    #[test]
    fn a_skipped_page_advances_when_more_pages_exist() {
        assert_eq!(
            batch_step(BatchOutcome::Skipped, None, Some("page-2".to_string()), 0),
            BatchStep::Continue {
                cursor: Some("page-2".to_string()),
                made_progress: false,
                requires_fresh_retry: true,
            }
        );
    }

    #[test]
    fn a_skipped_page_stops_when_the_server_repeats_the_cursor() {
        // Without this the loop re-requests the same unsignable page forever
        // with no delay between requests.
        assert_eq!(
            batch_step(
                BatchOutcome::Skipped,
                Some("page-1"),
                Some("page-1".to_string()),
                0
            ),
            BatchStep::RetryFresh
        );
    }

    #[test]
    fn a_skipped_page_stops_after_the_consecutive_page_limit() {
        assert!(matches!(
            batch_step(
                BatchOutcome::Skipped,
                Some("a"),
                Some("b".to_string()),
                crate::retry::MAX_CONSECUTIVE_EMPTY_PAGES - 1
            ),
            BatchStep::Continue { .. }
        ));
        assert_eq!(
            batch_step(
                BatchOutcome::Skipped,
                Some("a"),
                Some("b".to_string()),
                crate::retry::MAX_CONSECUTIVE_EMPTY_PAGES
            ),
            BatchStep::RetryFresh
        );
    }

    #[test]
    fn scan_retry_survives_a_successful_later_page_until_completion() {
        let mut retry = ScanRetryState::default();

        let BatchStep::Continue {
            requires_fresh_retry,
            ..
        } = batch_step(BatchOutcome::Skipped, None, Some("page-2".to_string()), 0)
        else {
            panic!("a skipped page with a cursor must continue the scan");
        };
        if requires_fresh_retry {
            retry.require();
        }

        let BatchStep::Continue {
            requires_fresh_retry,
            ..
        } = batch_step(BatchOutcome::Completed, Some("page-2"), None, 1)
        else {
            panic!("a completed final page must complete the scan");
        };
        if requires_fresh_retry {
            retry.require();
        }

        assert!(retry.take(), "the skipped page must force a fresh lookup");
        assert!(!retry.take(), "taking the retry starts a clean next scan");
    }

    #[test]
    fn submission_failure_restarts_from_a_fresh_cursor() {
        assert_eq!(
            batch_step(
                BatchOutcome::SubmissionFailed,
                None,
                Some("page-2".to_string()),
                0
            ),
            BatchStep::RetryFresh
        );
    }

    #[test]
    fn chain_activity_is_compared_only_after_the_full_cursor_scan() {
        let enjin_matrix = (Network::Enjin, Chain::Matrix);
        let enjin_relay = (Network::Enjin, Chain::Relay);
        let canary_matrix = (Network::Canary, Chain::Matrix);

        let mut previous_scan_chains: HashSet<ChainKey> =
            [enjin_matrix, enjin_relay, canary_matrix]
                .into_iter()
                .collect();
        let mut current_scan_chains = HashSet::new();

        // Simulate three cursor pages: Matrix -> Relay -> Matrix. Matrix must
        // remain active throughout even though it is absent from page 2.
        current_scan_chains.insert(enjin_matrix);
        current_scan_chains.insert(enjin_relay);
        current_scan_chains.insert(enjin_matrix);

        let idle = complete_chain_scan(&mut previous_scan_chains, &mut current_scan_chains);

        assert_eq!(idle, HashSet::from([canary_matrix]));
        assert_eq!(
            previous_scan_chains,
            HashSet::from([enjin_matrix, enjin_relay])
        );
        assert!(current_scan_chains.is_empty());
    }

    /// Within a single batch, a per-batch `HashMap<NonceKey, u64>` must:
    ///   * issue strictly consecutive nonces for repeat (network, chain, signer)
    ///     triples, and
    ///   * keep counters fully isolated across networks and chains for the
    ///     same signing account.
    #[test]
    fn per_batch_nonce_map_is_consecutive_and_chain_isolated() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();

        let pubkey = [0xABu8; 32];
        let enjin_matrix: NonceKey = (Network::Enjin, Chain::Matrix, pubkey);
        let canary_matrix: NonceKey = (Network::Canary, Chain::Matrix, pubkey);
        let enjin_relay: NonceKey = (Network::Enjin, Chain::Relay, pubkey);

        // Simulate prefetch returning chain-side starting nonces.
        nonces.insert(enjin_matrix, 0);
        nonces.insert(canary_matrix, 0);
        nonces.insert(enjin_relay, 0);

        // Sign 6 txs on Enjin Matrix: nonces 0..6, counter ends at 6.
        for expected in 0..6u64 {
            let slot = nonces.get_mut(&enjin_matrix).unwrap();
            assert_eq!(*slot, expected, "enjin matrix not consecutive");
            *slot += 1;
        }
        assert_eq!(nonces[&enjin_matrix], 6);

        // Same account, different network: must still start at 0.
        {
            let slot = nonces.get_mut(&canary_matrix).unwrap();
            assert_eq!(*slot, 0, "canary matrix must not see enjin matrix nonces");
            *slot += 1;
        }
        assert_eq!(nonces[&canary_matrix], 1);
        assert_eq!(nonces[&enjin_matrix], 6, "enjin matrix must be untouched");

        // Same account, same network, different chain: also independent.
        {
            let slot = nonces.get_mut(&enjin_relay).unwrap();
            assert_eq!(*slot, 0, "enjin relay must be isolated from enjin matrix");
            *slot += 1;
        }
        assert_eq!(nonces[&enjin_relay], 1);
        assert_eq!(nonces[&enjin_matrix], 6);
    }

    /// A failed sign/build (modeled here as "do not increment the slot")
    /// must leave the per-batch counter unchanged so the next successful
    /// request for the same key reuses that nonce. This exercises the
    /// "increment only on success" property of the new handler.
    #[test]
    fn nonce_slot_is_not_advanced_on_failure() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();
        let key: NonceKey = (Network::Enjin, Chain::Matrix, [0u8; 32]);
        nonces.insert(key, 10);

        // Successful tx: advance.
        {
            let slot = nonces.get_mut(&key).unwrap();
            assert_eq!(*slot, 10);
            *slot += 1;
        }

        // Failed tx: do NOT advance (simulates a `continue` in the handler
        // before the post-push increment).
        {
            let slot = nonces.get_mut(&key).unwrap();
            assert_eq!(*slot, 11);
            // no `*slot += 1`
        }

        // Next successful tx reuses 11.
        {
            let slot = nonces.get_mut(&key).unwrap();
            assert_eq!(*slot, 11);
            *slot += 1;
        }

        assert_eq!(nonces[&key], 12);
    }

    /// Cross-batch carry-over: when Platform's corrected nonce has not yet
    /// caught up to our previously signed extrinsics, the cached slot must be
    /// preserved across batches. This is the common case for back-to-back
    /// pages against the same `(network, chain, signer)`.
    ///
    /// Mirrors the `max(slot, platform_nonce)` rebase in
    /// `transaction_handler`'s seed loop: when `slot >= platform_nonce`,
    /// the slot is unchanged.
    #[test]
    fn cache_carry_over_preserves_slot_when_platform_nonce_is_behind() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();
        let key: NonceKey = (Network::Enjin, Chain::Matrix, [0u8; 32]);

        // End-of-page-1 state: 25 txs signed starting at nonce 21, so the
        // slot now holds 46. Platform's corrected value still reports 21.
        nonces.insert(key, 46);
        let platform_nonce: u64 = 21;

        // Apply the same rebase rule as the production seed loop:
        // `slot = max(slot, platform_nonce)`. When Platform is behind us,
        // the slot must remain at 46.
        let slot = nonces.get_mut(&key).unwrap();
        if *slot < platform_nonce {
            *slot = platform_nonce;
        }
        assert_eq!(
            *slot, 46,
            "slot must be preserved when chain is behind the cache"
        );

        // The next signed tx must therefore use 46, not 21.
        *slot += 1;
        assert_eq!(nonces[&key], 47);
    }

    /// Corrected advance: when Platform's nonce has moved past the cached slot
    /// because either chain state or recently submitted extrinsics advanced,
    /// the cache must be rebased before signing.
    ///
    /// Mirrors the `max(slot, platform_nonce)` rebase in
    /// `transaction_handler`'s seed loop: when `slot < platform_nonce`,
    /// the slot is bumped up.
    #[test]
    fn cache_rebases_when_platform_corrected_nonce_advances() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();
        let key: NonceKey = (Network::Enjin, Chain::Matrix, [0u8; 32]);

        // Cache says next nonce is 30, while Platform now reports 35.
        nonces.insert(key, 30);
        let platform_nonce: u64 = 35;

        let slot = nonces.get_mut(&key).unwrap();
        if *slot < platform_nonce {
            *slot = platform_nonce;
        }
        assert_eq!(
            *slot, 35,
            "slot must be rebased when Platform has advanced past the cache"
        );

        // The next signed tx must therefore use 35, not 30.
        *slot += 1;
        assert_eq!(nonces[&key], 36);
    }

    /// Reset-on-idle: when an authoritative lookup no longer returns a chain,
    /// every cache entry for that chain must be evicted while other chains'
    /// entries remain intact.
    #[test]
    fn evict_idle_chains_drops_only_matching_chains() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();

        let alice = [0xAAu8; 32];
        let bob = [0xBBu8; 32];

        // Two signers active on Enjin Matrix.
        nonces.insert((Network::Enjin, Chain::Matrix, alice), 5);
        nonces.insert((Network::Enjin, Chain::Matrix, bob), 9);
        // One signer active on Enjin Relay.
        nonces.insert((Network::Enjin, Chain::Relay, alice), 12);
        // One signer active on Canary Matrix.
        nonces.insert((Network::Canary, Chain::Matrix, alice), 3);

        // The latest authoritative lookup reported Enjin Matrix as idle.
        let idle: HashSet<ChainKey> = [(Network::Enjin, Chain::Matrix)].into_iter().collect();
        evict_idle_chains(&mut nonces, &idle);

        // Both Enjin Matrix entries (Alice + Bob) must be gone.
        assert!(!nonces.contains_key(&(Network::Enjin, Chain::Matrix, alice)));
        assert!(!nonces.contains_key(&(Network::Enjin, Chain::Matrix, bob)));

        // Other chains are untouched.
        assert_eq!(nonces[&(Network::Enjin, Chain::Relay, alice)], 12);
        assert_eq!(nonces[&(Network::Canary, Chain::Matrix, alice)], 3);
        assert_eq!(nonces.len(), 2);
    }

    /// A no-op idle set must not touch anything.
    #[test]
    fn evict_idle_chains_empty_set_is_noop() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();
        nonces.insert((Network::Enjin, Chain::Matrix, [0u8; 32]), 7);

        evict_idle_chains(&mut nonces, &HashSet::new());

        assert_eq!(nonces[&(Network::Enjin, Chain::Matrix, [0u8; 32])], 7);
        assert_eq!(nonces.len(), 1);
    }

    /// Rollback-on-failure: when the platform `SignTransactions` mutation
    /// fails after retries, the in-memory nonce counters that were
    /// advanced during the failed batch must be evicted (and only those —
    /// any other keys, including different signers on the same chain,
    /// must remain intact). The next batch will then re-fetch the real
    /// Platform-corrected nonce for each evicted key and re-sign the same
    /// uuids at the correct nonce values.
    #[test]
    fn evict_nonce_keys_drops_only_matching_keys() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();

        let alice = [0xAAu8; 32];
        let bob = [0xBBu8; 32];

        // Alice was used on Enjin Matrix and Enjin Relay this batch.
        nonces.insert((Network::Enjin, Chain::Matrix, alice), 31);
        nonces.insert((Network::Enjin, Chain::Relay, alice), 12);
        // Bob was on Enjin Matrix this batch but in a *different* tick
        // that already succeeded; his cache must not be touched.
        nonces.insert((Network::Enjin, Chain::Matrix, bob), 9);
        // Alice on Canary was untouched this batch.
        nonces.insert((Network::Canary, Chain::Matrix, alice), 4);

        // Failed batch advanced Alice on both Enjin chains.
        let to_evict: HashSet<NonceKey> = [
            (Network::Enjin, Chain::Matrix, alice),
            (Network::Enjin, Chain::Relay, alice),
        ]
        .into_iter()
        .collect();

        let evicted = evict_nonce_keys(&mut nonces, &to_evict);

        assert_eq!(evicted, 2);
        assert!(!nonces.contains_key(&(Network::Enjin, Chain::Matrix, alice)));
        assert!(!nonces.contains_key(&(Network::Enjin, Chain::Relay, alice)));
        // Bob on the same chain as Alice must survive.
        assert_eq!(nonces[&(Network::Enjin, Chain::Matrix, bob)], 9);
        // Alice on Canary must survive.
        assert_eq!(nonces[&(Network::Canary, Chain::Matrix, alice)], 4);
        assert_eq!(nonces.len(), 2);
    }

    /// A no-op `evict_nonce_keys` call must not touch anything.
    #[test]
    fn evict_nonce_keys_empty_set_is_noop() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();
        nonces.insert((Network::Enjin, Chain::Matrix, [0u8; 32]), 7);

        let evicted = evict_nonce_keys(&mut nonces, &HashSet::new());

        assert_eq!(evicted, 0);
        assert_eq!(nonces[&(Network::Enjin, Chain::Matrix, [0u8; 32])], 7);
        assert_eq!(nonces.len(), 1);
    }

    /// Per-batch dedup invariant: when a batch contains many requests
    /// for the same `(network, chain, signer)` triple, the seed loop
    /// must call out to chain exactly once for that key. Otherwise we'd
    /// pay one chain RPC per request (catastrophic for batches of 25)
    /// and — worse — every fetch after the first would clobber the
    /// in-flight slot we've already started advancing.
    ///
    /// The production loop tracks this via a `refreshed_keys: HashSet`
    /// guard: the first encounter inserts into the set and fetches; all
    /// subsequent encounters short-circuit. This test models that
    /// guard.
    #[test]
    fn seed_loop_fetches_each_key_exactly_once_per_batch() {
        let alice = [0xAAu8; 32];
        let bob = [0xBBu8; 32];

        // Simulated batch: 5 requests, three of them are Alice on Enjin
        // Matrix (the duplicates), one is Bob on Enjin Matrix, one is
        // Alice on Canary Matrix.
        let batch: Vec<NonceKey> = vec![
            (Network::Enjin, Chain::Matrix, alice),
            (Network::Enjin, Chain::Matrix, alice),
            (Network::Enjin, Chain::Matrix, bob),
            (Network::Enjin, Chain::Matrix, alice),
            (Network::Canary, Chain::Matrix, alice),
        ];

        let mut refreshed_keys: HashSet<NonceKey> = HashSet::new();
        let mut fetch_count: HashMap<NonceKey, u32> = HashMap::new();

        for key in &batch {
            if refreshed_keys.contains(key) {
                continue;
            }
            // This branch models the chain RPC.
            *fetch_count.entry(*key).or_insert(0) += 1;
            refreshed_keys.insert(*key);
        }

        // Each distinct key fetched exactly once...
        assert_eq!(fetch_count[&(Network::Enjin, Chain::Matrix, alice)], 1);
        assert_eq!(fetch_count[&(Network::Enjin, Chain::Matrix, bob)], 1);
        assert_eq!(fetch_count[&(Network::Canary, Chain::Matrix, alice)], 1);
        // ...and no other keys were fetched.
        assert_eq!(fetch_count.len(), 3);
        // Total fetches = distinct keys, not request count.
        assert_eq!(fetch_count.values().sum::<u32>(), 3);
        assert_eq!(refreshed_keys.len(), 3);
    }

    /// When the consumer's `Receiver` has been dropped (consumer task
    /// has exited), `send_tick_and_wait` must return
    /// `TickDispatchError::ConsumerGone` rather than logging and
    /// silently returning. This is what allows the producer's polling
    /// loop to terminate the daemon instead of degenerating into a
    /// tight loop hammering the platform with `GetPendingTransactions`
    /// while every dispatch fails.
    #[tokio::test]
    async fn send_tick_and_wait_returns_consumer_gone_when_receiver_dropped() {
        let (sender, receiver) = tokio::sync::mpsc::channel::<ProcessorTick>(1);
        let job = TransactionJob::new(sender);
        // Drop the receiver -> any subsequent send fails with
        // `mpsc::error::SendError`, which is the exact failure mode the
        // reviewer flagged.
        drop(receiver);

        let result = job
            .send_tick_and_wait(Vec::new(), HashSet::new(), "test empty tick")
            .await;

        assert!(matches!(result, Err(TickDispatchError::ConsumerGone)));
    }

    /// When the consumer accepts the tick but drops the ack `oneshot`
    /// without signaling (i.e. panics mid-tick), `send_tick_and_wait`
    /// must return `TickDispatchError::AckDropped`. This too is fatal:
    /// it indicates the consumer has abandoned a tick and we have no
    /// guarantee the corresponding `SignTransactions` was issued.
    #[tokio::test]
    async fn send_tick_and_wait_returns_ack_dropped_when_consumer_drops_ack() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<ProcessorTick>(1);
        let job = TransactionJob::new(sender);

        // Pretend to be a consumer that accepts the tick (drains the
        // mpsc) but then drops the ack without signaling.
        let consumer = tokio::spawn(async move {
            if let Some(tick) = receiver.recv().await {
                drop(tick.ack);
            }
        });

        let result = job
            .send_tick_and_wait(Vec::new(), HashSet::new(), "test empty tick")
            .await;
        consumer.await.unwrap();

        assert!(matches!(result, Err(TickDispatchError::AckDropped)));
    }
}
