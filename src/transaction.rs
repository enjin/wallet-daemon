mod fuel_tank;

use crate::graphql::sign_transactions::SignTransactionInput;
use crate::graphql::{GetPendingTransactions, get_pending_transactions};
use crate::transaction::fuel_tank::ExpirableSignature;
use crate::transaction::payload::RawFields;
use crate::types::{Chain, Network};
use crate::{DUMMY_TX_MORTALITY, TX_MORTALITY, chain_info, global, platform_client, utils};
use lru::LruCache;
use parity_scale_codec::Encode;
use payload::RawPayload;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subxt::config::DefaultExtrinsicParamsBuilder;
use subxt_signer::DeriveJunction;
use subxt_signer::sr25519::Keypair;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::interval;

const NO_TRANSACTIONS_MSG: &str = "No transactions present in the body";
const TRANSACTION_POLLER_MS: u64 = 6000;
const TRANSACTION_PAGE_SIZE: i64 = 25;

mod payload {
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

        loop {
            interval.tick().await;

            match self.get_pending_transactions().await {
                Ok(transaction_reqs) => {
                    if let Err(e) = self.sender.try_send(transaction_reqs) {
                        tracing::info!("Error sending transaction requests: {:?}", e);
                    }
                }
                Err(e) => {
                    if e.to_string() == NO_TRANSACTIONS_MSG {
                        tracing::info!("GetPendingTransactions: {}", NO_TRANSACTIONS_MSG,);
                    } else {
                        tracing::error!("Error: {:?}", e);
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
                        tracing::error!("Error creating TransactionRequest: {:?}", e);
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

    async fn transaction_handler(
        keypair: Keypair,
        nonce_tracker: Arc<Mutex<LruCache<String, u64>>>,
        requests: Vec<TransactionRequest>,
    ) {
        let mut inputs = Vec::with_capacity(requests.len());

        for request in requests {
            let TransactionRequest {
                request_id,
                external_id,
                network,
                chain,
                mut payload,
                fuel_tank_signer_external_id,
            } = request;
            tracing::info!("Received transaction request: #{request_id}");

            // get block number
            let Ok((block_number, block_hash, spec_version)) =
                chain_info::get_block_and_spec_version(network, chain).await
            else {
                tracing::error!("could not fetch block number");
                continue;
            };

            // check update metadata
            {
                let mut update_metadata = false;
                if let Some(local_spec_version) =
                    global::metadata_spec_version(network, chain).await
                {
                    if local_spec_version < spec_version {
                        update_metadata = true;
                    }
                } else {
                    update_metadata = true;
                }

                if update_metadata
                    && let Err(e) =
                        chain_info::update_metadata_and_substrate_client(network, chain).await
                {
                    tracing::error!("failed to update metadata for {network:?} {chain:?}: {e:?}");
                    continue;
                }
            }

            let signer = if let Some(external_id) = external_id {
                let derive_junction = match external_id.parse::<i64>() {
                    Ok(id) => DeriveJunction::soft(id),
                    Err(_) => DeriveJunction::soft(external_id),
                };

                keypair.derive([derive_junction])
            } else {
                keypair.clone()
            };

            tracing::info!(
                "Signing transaction #{} with account {}",
                request_id,
                hex::encode(signer.public_key().0)
            );

            let public_key = hex::encode(signer.public_key().0);
            let chain_nonce =
                match chain_info::get_account_nonce(network, chain, &signer.public_key()).await {
                    Ok(nonce) => nonce,
                    Err(e) => {
                        tracing::error!("failed to fetch nonce for {public_key} with error: {e:?}");
                        continue;
                    }
                };
            let correct_nonce: u64;
            {
                let mut tracker = nonce_tracker.lock().unwrap();
                let latest_nonce = tracker.get(&public_key).unwrap_or(&0u64);
                correct_nonce = *latest_nonce.max(&chain_nonce);
                let acc_format = trim_account(public_key.clone());
                tracing::warn!(
                    "Acc: {acc_format} - Using nonce: {correct_nonce:?} - Cached nonce: {latest_nonce:?} - Metadata nonce: {chain_nonce:?} - Next nonce: {:?}",
                    correct_nonce + 1
                );
                tracker.put(public_key.clone(), correct_nonce + 1);
            }

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
                let signer = if let Some(external_id) = fuel_tank_signer_external_id {
                    let derive_junction = match external_id.parse::<i64>() {
                        Ok(id) => DeriveJunction::soft(id),
                        Err(_) => DeriveJunction::soft(external_id),
                    };

                    keypair.derive([derive_junction])
                } else {
                    keypair.clone()
                };
                let signature = sp_core::sr25519::Signature::from_raw(signer.sign(&message).0);
                tracing::info!(
                    "fuel tanks - signed message {} with {} and got signature {}",
                    hex::encode(&message),
                    hex::encode(signer.public_key().0),
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
                            tracing::error!("Failed to sign dummy transaction: {:?}", e);
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::error!("Failed to create signed dummy transaction: {:?}", e);
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
                            tracing::error!("Failed to sign transaction: {:?}", e);
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::error!("Failed to create signed transaction: {:?}", e);
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
        }
        platform_client::sign_transactions(inputs).await;
    }

    async fn launch_job_scheduler(mut self) {
        let nonce_tracker: Arc<Mutex<LruCache<String, u64>>> =
            Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(1_000).unwrap())));

        while let Some(requests) = self.receiver.recv().await {
            tokio::spawn(Self::transaction_handler(
                self.keypair.clone(),
                Arc::clone(&nonce_tracker),
                requests,
            ));
        }
    }

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(self.launch_job_scheduler())
    }
}

fn trim_account(account: String) -> String {
    format!("0x{}...{}", &account[..4], &account[60..])
}
