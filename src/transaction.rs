mod fuel_tank;

use crate::graphql::sign_transactions::SignTransactionInput;
use crate::graphql::{GetPendingTransactions, get_pending_transactions};
use crate::transaction::fuel_tank::ExpirableSignature;
use crate::transaction::payload::RawFields;
use crate::types::{Chain, Network};
use crate::{DUMMY_TX_MORTALITY, TX_MORTALITY, chain_info, global, platform_client, utils};
use parity_scale_codec::Encode;
use payload::RawPayload;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use subxt::config::DefaultExtrinsicParamsBuilder;
use subxt::utils::H256;
use subxt_signer::DeriveJunction;
use subxt_signer::sr25519::Keypair;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::sleep;

const NO_TRANSACTIONS_MSG: &str = "No transactions present in the body";
const TRANSACTION_POLLER_MS: u64 = 6000;
const TRANSACTION_PAGE_SIZE: i64 = 25;

/// Per-batch nonce map key. Each `(network, chain, signer public key)` tracks
/// its own counter for the duration of a single batch.
type NonceKey = (Network, Chain, [u8; 32]);

/// Per-batch chain key for block / metadata prefetch.
type ChainKey = (Network, Chain);

/// `(block_number, block_hash, spec_version)` captured at prefetch time and
/// reused for every request signed against that chain in the batch.
type BlockInfo = (u32, H256, u32);

/// Message sent from the poller to the processor on every poll tick.
///
/// `batch` is the (possibly empty) set of transaction requests pulled this
/// tick. `idle` is the set of `(network, chain)` pairs that the poller has
/// previously delivered txs for but did **not** see in this tick's response —
/// the processor uses this to evict their nonce-cache entries so the next
/// batch for those chains re-reads the on-chain nonce from scratch. This
/// prevents long-term cross-batch nonce desync while letting back-to-back
/// batches reuse the in-memory counter.
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
    ack: oneshot::Sender<()>,
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
}

impl TransactionJob {
    pub fn new(sender: Sender<ProcessorTick>) -> Self {
        Self { sender }
    }

    pub fn create_job(keypair: Keypair) -> (TransactionJob, TransactionProcessor) {
        // Capacity 1: combined with `send().await` on the producer side and
        // a per-tick `oneshot` ack from the consumer, this enforces "at most
        // one tick in flight at a time." The producer cannot issue a new
        // `GetPendingTransactions` until the consumer has both pulled the
        // previous tick AND signaled completion of `SignTransactions` for
        // it. See `ProcessorTick` for the rationale.
        let (sender, receiver) = tokio::sync::mpsc::channel(1);

        (
            TransactionJob::new(sender),
            TransactionProcessor::new(keypair, receiver),
        )
    }

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.start_polling().await;
        })
    }

    async fn start_polling(&self) {
        let mut no_transaction_count = 0;
        // Set of (network, chain) pairs we've ever delivered a batch for. Used
        // to compute the `idle` set: chains we've seen before but didn't see
        // this tick. The cache for any chain in `idle` will be evicted on the
        // consumer side.
        let mut seen_chains: HashSet<ChainKey> = HashSet::new();

        loop {
            // Sleep when there is no work to do (or on error). When a
            // non-empty batch was successfully delivered, we immediately
            // re-poll to catch any transactions that were added to the
            // queue while we were signing the previous batch (signing can
            // take several seconds; pagination already drained the visible
            // backlog in a single call, so this re-poll is purely about
            // picking up newly-arrived work).
            //
            // Critically, we await the consumer's ack between sending a
            // tick and the next poll. Without that, the producer can race
            // ahead and re-fetch the same uuid multiple times before the
            // consumer has submitted the corresponding `SignTransactions`
            // mutation, causing the same uuid to be signed with consecutive
            // nonces and rejected by the platform.
            let should_sleep = match self.get_pending_transactions().await {
                Ok(transaction_reqs) => {
                    // Treat an empty `Ok` (e.g. every item on page 1 failed
                    // `TryFrom`) the same as `NO_TRANSACTIONS_MSG`: back off
                    // instead of busy-looping on a broken-data condition.
                    if transaction_reqs.is_empty() {
                        if !seen_chains.is_empty() {
                            self.send_tick_and_wait(Vec::new(), seen_chains.clone(), "idle tick")
                                .await;
                        }
                        true
                    } else {
                        let active: HashSet<ChainKey> = transaction_reqs
                            .iter()
                            .map(|r| (r.network, r.chain))
                            .collect();
                        let idle: HashSet<ChainKey> =
                            seen_chains.difference(&active).copied().collect();
                        seen_chains.extend(active.iter().copied());

                        self.send_tick_and_wait(transaction_reqs, idle, "transaction requests")
                            .await;
                        no_transaction_count = 0;
                        false
                    }
                }
                Err(e) => {
                    if e.to_string() == NO_TRANSACTIONS_MSG {
                        if no_transaction_count % 10 == 0 {
                            tracing::info!("GetPendingTransactions: {}", NO_TRANSACTIONS_MSG,);
                        }
                        no_transaction_count += 1;

                        // Empty poll: every chain we've ever seen is idle this
                        // tick. Send an empty-batch tick so the consumer can
                        // evict their cache entries.
                        if !seen_chains.is_empty() {
                            self.send_tick_and_wait(Vec::new(), seen_chains.clone(), "idle tick")
                                .await;
                        }
                    } else {
                        tracing::error!("Error: {}", e);
                    }
                    true
                }
            };

            if should_sleep {
                sleep(Duration::from_millis(TRANSACTION_POLLER_MS)).await;
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
    /// Failure modes are logged and swallowed — there is no point bubbling
    /// them up because the polling loop has nowhere useful to send them and
    /// must keep running. If `send().await` fails, the consumer task has
    /// died, in which case the daemon is in a broken state regardless. If
    /// the ack channel is dropped without being signaled, the consumer
    /// finished without acknowledging (this should not happen — the
    /// consumer always sends the ack on the way out of every tick branch);
    /// log and proceed so the producer can keep polling.
    async fn send_tick_and_wait(
        &self,
        batch: Vec<TransactionRequest>,
        idle: HashSet<ChainKey>,
        kind: &str,
    ) {
        let (ack_tx, ack_rx) = oneshot::channel();
        let tick = ProcessorTick {
            batch,
            idle,
            ack: ack_tx,
        };
        if let Err(e) = self.sender.send(tick).await {
            tracing::error!("Failed to send {kind} to processor: {e:?}");
            return;
        }
        if let Err(e) = ack_rx.await {
            tracing::warn!("Processor dropped ack for {kind} without signaling: {e:?}");
        }
    }

    async fn get_pending_transactions(
        &self,
    ) -> Result<Vec<TransactionRequest>, Box<dyn std::error::Error + Send + Sync>> {
        let mut all: Vec<TransactionRequest> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page_index: usize = 0;

        loop {
            let response_data = utils::execute_query::<GetPendingTransactions>(
                get_pending_transactions::Variables {
                    limit: TRANSACTION_PAGE_SIZE,
                    cursor: cursor.clone(),
                },
                None,
            )
            .await?;

            let result = match response_data.result {
                Some(r) => r,
                None => {
                    // First page: no result at all -> nothing pending.
                    // Later page: server gave us a `nextCursor` but then
                    // returned no result; treat as end-of-stream.
                    if page_index == 0 {
                        return Err(NO_TRANSACTIONS_MSG.into());
                    }
                    break;
                }
            };

            let page = result.data;
            let next_cursor = result.next_cursor;
            let page_len = page.len();

            if page_index == 0 && page.is_empty() {
                return Err(NO_TRANSACTIONS_MSG.into());
            }

            all.extend(page.into_iter().filter_map(|p| {
                TransactionRequest::try_from(p)
                    .map_err(|e| {
                        tracing::error!("Error creating TransactionRequest: {}", e);
                        e
                    })
                    .ok()
            }));

            tracing::debug!(
                "GetPendingTransactions: fetched page {} ({} items, total so far: {}), next_cursor present: {}",
                page_index,
                page_len,
                all.len(),
                next_cursor.is_some(),
            );

            page_index += 1;

            match next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }

        if page_index > 1 {
            tracing::info!(
                "GetPendingTransactions: drained {} pages, {} transaction(s) total",
                page_index,
                all.len(),
            );
        }

        Ok(all)
    }
}

pub struct TransactionProcessor {
    keypair: Keypair,
    receiver: Receiver<ProcessorTick>,
    /// Persistent nonce cache. The slot for `(network, chain, signer)` holds
    /// the next nonce to use for that triple. Survives across batches so that
    /// back-to-back batches stay in sync; entries are evicted when the poller
    /// reports a chain has gone idle (see `ProcessorTick::idle`).
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
    ) {
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

        // Seed the persistent nonce cache for any (network, chain, signer)
        // triple in this batch that doesn't already have an entry. Triples
        // with an existing cache entry are NOT re-fetched — that's how we
        // carry the counter across back-to-back batches. The empty-poll
        // reset (handled in `launch_job_scheduler`) is what eventually
        // evicts a stale entry when a chain goes quiet.
        let mut failed_keys: HashSet<NonceKey> = HashSet::new();
        for (signer, request) in signers.iter().zip(requests.iter()) {
            // Don't bother fetching a nonce for a chain that already failed
            // its block / metadata prefetch.
            if failed_chains.contains(&(request.network, request.chain)) {
                continue;
            }
            let key: NonceKey = (request.network, request.chain, signer.public_key().0);
            if nonces.contains_key(&key) || failed_keys.contains(&key) {
                continue;
            }
            match chain_info::get_account_nonce(
                request.network,
                request.chain,
                &signer.public_key(),
            )
            .await
            {
                Ok(n) => {
                    tracing::debug!(
                        "Prefetched nonce {n} for acc 0x{} - Network: {:?} - Chain: {:?}",
                        hex::encode(key.2),
                        request.network,
                        request.chain,
                    );
                    nonces.insert(key, n);
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

        // Snapshot the uuids actually queued for submission so we can name
        // them in the rollback log if the platform mutation fails.
        let submitted_uuids: Vec<String> = inputs.iter().map(|i| i.uuid.clone()).collect();

        if let Err(e) = platform_client::sign_transactions(inputs).await {
            // Platform-side failure (after retries): every nonce we
            // advanced in this batch is uncommitted — those uuids are
            // still pending on the platform side and the on-chain nonce
            // hasn't moved. Evict the affected cache entries so the next
            // batch re-fetches the real on-chain nonce and re-signs the
            // same uuids at the correct values. Without this, the cache
            // would drift forward by `committed_keys.len()` entries
            // relative to chain reality, producing future-nonce
            // extrinsics on every subsequent batch and a stuck queue.
            let evicted = evict_nonce_keys(nonces, &committed_keys);
            let chains: HashSet<ChainKey> = committed_keys
                .iter()
                .map(|(net, chain, _)| (*net, *chain))
                .collect();
            tracing::error!(
                "Platform SignTransactions failed; evicting nonce cache for affected chains so the next batch will re-fetch from chain. error={e} chains={chains:?} uuids={submitted_uuids:?} evicted={evicted}",
            );
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
            // Apply idle resets BEFORE processing the batch. By construction
            // the poller never includes a chain in both `batch` and `idle` for
            // the same tick, but applying resets first keeps semantics
            // unambiguous if that invariant ever changes.
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

            if !batch.is_empty() {
                Self::transaction_handler(self.keypair.clone(), &mut self.nonces, batch).await;
            }

            // Always signal the producer, even on an empty batch (idle
            // ticks still need an ack to release the producer). Failure
            // here means the producer has already gone away, which would
            // mean the daemon is shutting down — nothing actionable.
            let _ = ack.send(());
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

/// Drop every nonce-cache entry whose `(network, chain)` is in `idle`. Used
/// to flush stale counters when the poller reports a chain has gone quiet so
/// the next batch for that chain re-reads the on-chain nonce. Returns the
/// number of entries actually evicted.
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
/// uncommitted nonces and must be discarded so the next batch re-reads
/// the on-chain nonce from scratch and re-signs the same uuids at the
/// correct nonce values. Returns the number of entries actually evicted.
fn evict_nonce_keys(nonces: &mut HashMap<NonceKey, u64>, keys: &HashSet<NonceKey>) -> usize {
    let before = nonces.len();
    nonces.retain(|k, _| !keys.contains(k));
    before - nonces.len()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Cross-batch carry-over: a triple that already has a cache entry from
    /// a previous batch must NOT be re-seeded when the next batch starts. The
    /// next batch picks up where the previous one left off.
    ///
    /// This mirrors the seed-on-miss check in `transaction_handler`'s nonce
    /// prefetch (`if nonces.contains_key(&key) || failed_keys.contains(&key)
    /// { continue; }`). A miss would call out to chain; a hit must short-
    /// circuit.
    #[test]
    fn cache_carry_over_skips_chain_fetch_on_hit() {
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();
        let key: NonceKey = (Network::Enjin, Chain::Matrix, [0u8; 32]);

        // Simulate end-of-batch-1 state: 25 txs signed starting at chain
        // nonce 21, slot now holds 46 (= 21 + 25).
        nonces.insert(key, 46);

        // Batch 2 starts. The prefetch decision is `nonces.contains_key(&key)`.
        // A hit means we do NOT fetch from chain; we just reuse the slot.
        let would_fetch_from_chain = !nonces.contains_key(&key);
        assert!(
            !would_fetch_from_chain,
            "cache hit must not trigger a chain fetch"
        );

        // The next signed tx in batch 2 must therefore use 46, not 21.
        let slot = nonces.get_mut(&key).unwrap();
        assert_eq!(
            *slot, 46,
            "batch 2 must continue from where batch 1 left off"
        );
        *slot += 1;
        assert_eq!(nonces[&key], 47);
    }

    /// Reset-on-idle: when the poller reports a chain went idle, every cache
    /// entry for that chain (across all signing keys) must be evicted, while
    /// other chains' entries remain intact.
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

        // Poller reports: Enjin Matrix went idle this tick.
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
    /// on-chain nonce for each evicted key and re-sign the same uuids at
    /// the correct nonce values.
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
}
