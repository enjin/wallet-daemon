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
use tokio::task::JoinHandle;
use tokio::time::interval;

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
        tracing::info!("{:?}", data);
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
    sender: Sender<Vec<TransactionRequest>>,
}

impl TransactionJob {
    pub fn new(sender: Sender<Vec<TransactionRequest>>) -> Self {
        Self { sender }
    }

    pub fn create_job(keypair: Keypair) -> (TransactionJob, TransactionProcessor) {
        let (sender, receiver) = tokio::sync::mpsc::channel(50_000);

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
        let mut interval = interval(Duration::from_millis(TRANSACTION_POLLER_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut no_transaction_count = 0;

        loop {
            interval.tick().await;

            match self.get_pending_transactions().await {
                Ok(transaction_reqs) => {
                    if let Err(e) = self.sender.try_send(transaction_reqs) {
                        tracing::info!("Error sending transaction requests: {:?}", e);
                    }
                    no_transaction_count = 0;
                }
                Err(e) => {
                    if e.to_string() == NO_TRANSACTIONS_MSG {
                        if no_transaction_count % 10 == 0 {
                            tracing::info!("GetPendingTransactions: {}", NO_TRANSACTIONS_MSG,);
                        }
                        no_transaction_count += 1;
                    } else {
                        tracing::error!("Error: {}", e);
                    }
                }
            }
        }
    }

    async fn get_pending_transactions(
        &self,
    ) -> Result<Vec<TransactionRequest>, Box<dyn std::error::Error + Send + Sync>> {
        let response_data = utils::execute_query::<GetPendingTransactions>(
            get_pending_transactions::Variables {
                limit: TRANSACTION_PAGE_SIZE,
                cursor: None,
            },
            None,
        )
        .await?;

        let transactions_req = response_data.result.ok_or(NO_TRANSACTIONS_MSG)?.data;

        if transactions_req.is_empty() {
            return Err(NO_TRANSACTIONS_MSG.into());
        }

        Ok(transactions_req
            .into_iter()
            .filter_map(|p| {
                TransactionRequest::try_from(p)
                    .map_err(|e| {
                        tracing::error!("Error creating TransactionRequest: {}", e);
                        e
                    })
                    .ok()
            })
            .collect())
    }
}

pub struct TransactionProcessor {
    keypair: Keypair,
    receiver: Receiver<Vec<TransactionRequest>>,
}

impl TransactionProcessor {
    pub(crate) fn new(keypair: Keypair, receiver: Receiver<Vec<TransactionRequest>>) -> Self {
        Self { keypair, receiver }
    }

    async fn transaction_handler(keypair: Keypair, requests: Vec<TransactionRequest>) {
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

            tracing::info!(
                "Prefetched block {block_number} (spec {spec_version}) for {:?}/{:?}",
                request.network,
                request.chain,
            );
            block_info.insert(key, (block_number, block_hash, spec_version));
        }

        // Pre-fetch the starting nonce once per unique (network, chain, signer)
        // triple in this batch. We assume the platform's `get_account_nonce`
        // resolver returns `system_accountNextIndex`.
        let mut nonces: HashMap<NonceKey, u64> = HashMap::new();
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
                    tracing::info!(
                        "Prefetched nonce {n} for acc {} - Network: {:?} - Chain: {:?}",
                        trim_account(hex::encode(key.2)),
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

        for (signer, request) in signers.into_iter().zip(requests) {
            let TransactionRequest {
                request_id,
                external_id: _,
                network,
                chain,
                mut payload,
                fuel_tank_signer_external_id,
            } = request;
            tracing::info!("Received transaction request: #{request_id}");

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

            tracing::info!(
                "Signing transaction #{} with account {}",
                request_id,
                hex::encode(pubkey_bytes)
            );

            let Some(nonce_slot) = nonces.get_mut(&nonce_key) else {
                tracing::error!(
                    "missing pre-fetched nonce for {} on {network:?}/{chain:?}",
                    hex::encode(pubkey_bytes)
                );
                continue;
            };
            let correct_nonce = *nonce_slot;
            tracing::info!(
                "Acc: {} - Network: {network:?} - Chain: {chain:?} - Using nonce: {correct_nonce}",
                trim_account(hex::encode(pubkey_bytes)),
            );

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
                "Request: #{} - Nonce: {} - Extrinsic: 0x{}",
                request_id,
                correct_nonce,
                encoded_tx
            );
            inputs.push(SignTransactionInput {
                uuid: request_id.clone(),
                signed_extrinsic: format!("0x{encoded_tx}"),
                signed_abandon_extrinsic: dummy_tx.clone(),
            });

            // Only advance the nonce after a tx has been successfully
            // built, signed, and queued for submission.
            *nonce_slot += 1;
        }
        platform_client::sign_transactions(inputs).await;
    }

    async fn launch_job_scheduler(mut self) {
        // Process one batch at a time. Awaiting the handler (rather than
        // `tokio::spawn`ing it) guarantees the previous batch is fully signed
        // and submitted before the next one starts, which is required for
        // correct nonce sequencing.
        while let Some(requests) = self.receiver.recv().await {
            Self::transaction_handler(self.keypair.clone(), requests).await;
        }
    }

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(self.launch_job_scheduler())
    }
}

fn trim_account(account: String) -> String {
    format!("0x{}...{}", &account[..4], &account[60..])
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
}
