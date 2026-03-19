use crate::graphql::sign_transactions::SignTransactionInput;
use crate::graphql::sign_transactions::TransactionStateEnum;
use crate::graphql::{get_pending_transactions, GetPendingTransactions};
use crate::subscription::Network;
use crate::transaction::fuel_tank::ExpirableSignature;
use crate::{platform_client, SubscriptionParams, DUMMY_TX_MORTALITY, TX_MORTALITY};
use backoff::exponential::ExponentialBackoff;
use backoff::SystemClock;
use graphql_client::GraphQLQuery;
use lru::LruCache;
use parity_scale_codec::Encode;
use reqwest::{Client, Response};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subxt::config::DefaultExtrinsicParamsBuilder;
use subxt::{tx::TxStatus, OnlineClient, PolkadotConfig};
use subxt_signer::sr25519::Keypair;
use subxt_signer::DeriveJunction;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};

const NO_TRANSACTIONS_MSG: &str = "No transactions present in the body";
const BLOCK_TIME_MS: u64 = 12000;
const TRANSACTION_POLLER_MS: u64 = 6000;
const TRANSACTION_PAGE_SIZE: i64 = 25;

struct Wrapper(Vec<u8>);

struct SubmitResult {
    hash: String,
    correct_nonce: u64,
    encoded_tx: String,
}

impl subxt::tx::Payload for Wrapper {
    fn encode_call_data_to(
        &self,
        _metadata: &subxt::Metadata,
        out: &mut Vec<u8>,
    ) -> Result<(), subxt::ext::subxt_core::Error> {
        out.extend_from_slice(&self.0);
        Ok(())
    }
}

#[derive(Clone)]
pub struct TransactionRequest {
    request_id: String,
    external_id: Option<String>,
    payload: Vec<u8>,
    /// If this is Some, the extrinsic is a dispatch from fuel tanks and needs the signature added
    pub fuel_tank_owner_external_id: Option<Option<String>>,
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
            payload: hex::decode(data.encoded_data.split('x').nth(1).unwrap())?,
            fuel_tank_owner_external_id: data
                .should_sign_fuel_tank
                .then_some(data.fuel_tank_owner_external_id),
        })
    }
}

#[derive(Debug)]
pub struct TransactionJob {
    client: Client,
    sender: Sender<Vec<TransactionRequest>>,
    platform_url: String,
    platform_token: String,
    network: Arc<Network>,
}

impl TransactionJob {
    pub fn new(
        client: Client,
        sender: Sender<Vec<TransactionRequest>>,
        platform_url: String,
        platform_token: String,
        network: Arc<Network>,
    ) -> Self {
        Self {
            client,
            sender,
            platform_url,
            platform_token,
            network,
        }
    }

    pub fn create_job(
        rpc: Arc<OnlineClient<PolkadotConfig>>,
        block_sub: Arc<SubscriptionParams>,
        keypair: Keypair,
        platform_url: String,
        platform_token: String,
    ) -> (TransactionJob, TransactionProcessor) {
        let (sender, receiver) = tokio::sync::mpsc::channel(50_000);
        let network = block_sub.get_network();

        (
            TransactionJob::new(
                Client::new(),
                sender,
                platform_url.clone(),
                platform_token.clone(),
                network,
            ),
            TransactionProcessor::new(
                rpc,
                Client::new(),
                keypair,
                receiver,
                platform_url,
                platform_token,
            ),
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
                        tracing::info!(
                            "MarkAndListPendingTransactions: {} for {}",
                            NO_TRANSACTIONS_MSG,
                            self.network
                        );
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
        let res = GetPendingTransactions::build_query(get_pending_transactions::Variables {
            // TODO: get these from config
            network: crate::NETWORK.into(),
            chain: crate::CHAIN.into(),
            limit: TRANSACTION_PAGE_SIZE,
            cursor: None,
        });

        let res = self
            .client
            .post(&self.platform_url)
            .header("Authorization", &self.platform_token)
            .json(&res)
            .send()
            .await?;

        self.extract_transaction_requests(res).await
    }

    async fn extract_transaction_requests(
        &self,
        transactions_res: Response,
    ) -> Result<Vec<TransactionRequest>, Box<dyn std::error::Error + Send + Sync>> {
        let response_body: graphql_client::Response<get_pending_transactions::ResponseData> =
            transactions_res.json().await?;

        let response_data = response_body.data.ok_or(NO_TRANSACTIONS_MSG)?;
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
    chain_client: Arc<OnlineClient<PolkadotConfig>>,
    platform_client: Client,
    keypair: Keypair,
    receiver: Receiver<Vec<TransactionRequest>>,
    platform_url: String,
    platform_token: String,
}

impl TransactionProcessor {
    pub(crate) fn new(
        rpc: Arc<OnlineClient<PolkadotConfig>>,
        client: Client,
        keypair: Keypair,
        receiver: Receiver<Vec<TransactionRequest>>,
        platform_url: String,
        platform_token: String,
    ) -> Self {
        Self {
            chain_client: rpc,
            platform_client: client,
            keypair,
            receiver,
            platform_url,
            platform_token,
        }
    }

    async fn submit_and_watch(
        platform_client: Client,
        platform_url: String,
        platform_token: String,
        chain_client: Arc<OnlineClient<PolkadotConfig>>,
        keypair: Keypair,
        request_id: String,
        payload: Vec<u8>,
        correct_nonce: u64,
        encoded_tx: String,
        dummy_tx: String,
    ) -> Result<SubmitResult, Box<dyn std::error::Error + Send + Sync>> {
        let params = DefaultExtrinsicParamsBuilder::new()
            .nonce(correct_nonce)
            .mortal(64)
            .build();

        let signed_tx = chain_client
            .tx()
            .create_signed(&Wrapper(payload), &keypair, params)
            .await?;
        tracing::info!(
            "Request: #{} - Nonce: {} - Extrinsic: 0x{}",
            request_id,
            correct_nonce,
            encoded_tx
        );

        let mut transaction = signed_tx.submit_and_watch().await?;
        while let Some(status) = transaction.next().await {
            match status? {
                TxStatus::Validated => {
                    let trimmed = trim_account(hex::encode(keypair.public_key().0));
                    tracing::info!(
                        "Sent transaction #{} with nonce {} signed by {}",
                        request_id,
                        correct_nonce,
                        trimmed
                    );
                }
                TxStatus::Invalid { message } => {
                    tracing::error!("Transaction #{} is INVALID: {:?}", request_id, message);
                }
                TxStatus::Broadcasted => {
                    tracing::info!("Transaction #{} has been BROADCASTED", request_id);

                    platform_client::sign_transactions(
                        platform_client.clone(),
                        platform_url.clone(),
                        platform_token.clone(),
                        SignTransactionInput {
                            uuid: request_id.clone(),
                            signed_extrinsic: format!("0x{encoded_tx}"),
                            nonce: correct_nonce as i64,
                            state: TransactionStateEnum::BROADCAST,
                            signed_abandon_extrinsic: dummy_tx.clone(),
                        },
                    )
                    .await;
                }
                TxStatus::InBestBlock(block) => {
                    tracing::info!(
                        "Transaction #{} is now InBestBlock: {:?}",
                        request_id,
                        block.block_hash()
                    );
                    return Ok(SubmitResult {
                        hash: hex::encode(block.extrinsic_hash().0),
                        correct_nonce,
                        encoded_tx,
                    });
                }
                TxStatus::NoLongerInBestBlock => {
                    tracing::error!("Transaction #{} no longer InBestBlock", request_id)
                }
                TxStatus::Dropped { message } => {
                    tracing::error!(
                        "Transaction #{} has been DROPPED: {:?}",
                        request_id,
                        message
                    )
                }
                TxStatus::InFinalizedBlock(in_block) => tracing::info!(
                    "Transaction #{} with hash {:?} was included at block: {:?}",
                    request_id,
                    in_block.extrinsic_hash(),
                    in_block.block_hash()
                ),
                TxStatus::Error { message } => {
                    tracing::error!("Transaction #{} has an ERROR: {:?}", request_id, message)
                }
            }
        }

        Err(format!("Transaction #{} could not be signed or sent", request_id).into())
    }

    async fn transaction_handler(
        chain_client: Arc<OnlineClient<PolkadotConfig>>,
        platform_client: Client,
        keypair: Keypair,
        nonce_tracker: Arc<Mutex<LruCache<String, u64>>>,
        platform_url: String,
        platform_token: String,
        requests: Vec<TransactionRequest>,
    ) {
        for request in requests {
            let TransactionRequest {
                request_id,
                external_id,
                mut payload,
                fuel_tank_owner_external_id,
            } = request;
            tracing::info!("Received transaction request: #{request_id}");

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
            let chain_nonce = chain_client
                .tx()
                .account_nonce(&signer.public_key().into())
                .await
                .unwrap();
            let correct_nonce: u64;
            {
                let mut tracker = nonce_tracker.lock().unwrap();
                let latest_nonce = tracker.get(&public_key).unwrap_or(&0u64);
                correct_nonce = *latest_nonce.max(&chain_nonce);
                let acc_format = trim_account(public_key.clone());
                tracing::warn!("Acc: {acc_format} - Using nonce: {correct_nonce:?} - Cached nonce: {latest_nonce:?} - Metadata nonce: {chain_nonce:?} - Next nonce: {:?}", correct_nonce + 1);
                tracker.put(public_key.clone(), correct_nonce + 1);
            }

            if let Some(fuel_tank_owner_external_id) = fuel_tank_owner_external_id {
                // expiration block is needed for the signature
                let expiration_block = match chain_client.blocks().at_latest().await {
                    Ok(block) => block.number() + TX_MORTALITY as u32,
                    Err(e) => {
                        tracing::error!("failed to get block number: {e}");
                        continue;
                    }
                };

                // create message to be signed
                let mut message = payload.clone();
                message.extend_from_slice(public_key.as_bytes());
                message.extend_from_slice(&expiration_block.encode());

                // sign by the fuel tank external id if it exists
                let signer = if let Some(external_id) = fuel_tank_owner_external_id {
                    let derive_junction = match external_id.parse::<i64>() {
                        Ok(id) => DeriveJunction::soft(id),
                        Err(_) => DeriveJunction::soft(external_id),
                    };

                    keypair.derive([derive_junction])
                } else {
                    keypair.clone()
                };
                let signature = sp_core::sr25519::Signature::from_raw(signer.sign(&message).0);

                let settings = fuel_tank::DispatchSettings {
                    signature: Some(ExpirableSignature {
                        signature,
                        expiry_block: expiration_block,
                    }),
                    ..Default::default()
                };

                tracing::info!("payload before fuel tank: {}", hex::encode(&payload));

                // remove the last byte of the payload because it is the settings param, and we are
                // replacing it
                payload.pop();

                // append to the payload. This is fine because settings is the last param of the extrinsic
                payload.extend_from_slice(&Some(settings).encode());
                tracing::info!("fuel tank modified payload: {}", hex::encode(&payload));
            }

            let params = DefaultExtrinsicParamsBuilder::new()
                .nonce(correct_nonce)
                .mortal(TX_MORTALITY)
                .build();

            let signed_tx = match chain_client
                .tx()
                .create_signed(&Wrapper(payload.clone()), &signer, params)
                .await
            {
                Ok(tx) => tx,
                Err(e) => {
                    tracing::error!("Failed to create signed transaction: {:?}", e);
                    continue;
                }
            };
            let dummy_tx = {
                // this is system.remark with empty value: 0x000000
                let payload = vec![0, 0, 0];
                let params = DefaultExtrinsicParamsBuilder::new()
                    .nonce(correct_nonce)
                    .mortal(DUMMY_TX_MORTALITY)
                    .build();
                let signed_dummy_tx = match chain_client
                    .tx()
                    .create_signed(&Wrapper(payload), &signer, params)
                    .await
                {
                    Ok(tx) => tx,
                    Err(e) => {
                        tracing::error!("Failed to create signed dummy transaction: {:?}", e);
                        continue;
                    }
                };
                format!("0x{}", hex::encode(signed_dummy_tx.encoded()))
            };
            let encoded_tx = hex::encode(signed_tx.encoded());

            tracing::info!(
                "Request: #{} - Nonce: {} - Extrinsic: 0x{}",
                request_id,
                correct_nonce,
                encoded_tx
            );

            let res = backoff::future::retry(Self::default_backoff(), || async {
                match Self::submit_and_watch(
                    platform_client.clone(),
                    platform_url.clone(),
                    platform_token.clone(),
                    Arc::clone(&chain_client),
                    signer.clone(),
                    request_id.clone(),
                    payload.clone(),
                    correct_nonce,
                    encoded_tx.clone(),
                    dummy_tx.clone(),
                )
                .await
                {
                    Ok(result) => Ok(result),
                    Err(e) => {
                        nonce_tracker
                            .lock()
                            .unwrap()
                            .put(hex::encode(signer.public_key().0), 0);
                        tracing::info!(
                            "Resetting cached nonce from {} to 0",
                            hex::encode(signer.public_key().0)
                        );
                        tracing::error!(
                            "Error submitting transaction #{} from account {} payload: 0x{}",
                            request_id,
                            trim_account(hex::encode(signer.public_key().0)),
                            hex::encode(payload.clone())
                        );
                        tracing::error!("{:?}", e);
                        Err(backoff::Error::transient(e))
                    }
                }
            })
            .await;

            let signing_account = hex::encode(signer.public_key().0);
            let account = format!("0x{signing_account}");

            match res {
                Ok(SubmitResult {
                    hash,
                    correct_nonce,
                    encoded_tx,
                }) => {
                    let trimmed_hash = trim_account(hash.clone());
                    let trimmed_account = trim_account(account.clone());

                    tracing::info!(
                        "Transaction #{} hash {} signed with account {} setting it to EXECUTED",
                        request_id,
                        trimmed_hash,
                        trimmed_account
                    );

                    platform_client::sign_transactions(
                        platform_client.clone(),
                        platform_url.clone(),
                        platform_token.clone(),
                        SignTransactionInput {
                            uuid: request_id.clone(),
                            signed_extrinsic: format!("0x{encoded_tx}"),
                            nonce: correct_nonce as i64,
                            state: TransactionStateEnum::EXECUTED,
                            signed_abandon_extrinsic: dummy_tx.clone(),
                        },
                    )
                    .await;
                }
                Err(_) => {
                    tracing::error!(
                        "Transaction #{} failed to sign with account {} setting it to ABANDONED",
                        request_id,
                        trim_account(account.clone())
                    );

                    platform_client::sign_transactions(
                        platform_client.clone(),
                        platform_url.clone(),
                        platform_token.clone(),
                        SignTransactionInput {
                            uuid: request_id.clone(),
                            signed_extrinsic: format!("0x{encoded_tx}"),
                            nonce: correct_nonce as i64,
                            state: TransactionStateEnum::ABANDONED,
                            signed_abandon_extrinsic: dummy_tx.clone(),
                        },
                    )
                    .await;
                }
            }
        }
    }

    fn default_backoff() -> ExponentialBackoff<SystemClock> {
        let setting = backoff::ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_secs(6))
            .with_randomization_factor(0.2)
            .with_multiplier(2.0)
            .with_max_elapsed_time(Some(Duration::from_secs(120)))
            .build();
        setting
    }

    async fn launch_job_scheduler(mut self) {
        let nonce_tracker: Arc<Mutex<LruCache<String, u64>>> =
            Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(1_000).unwrap())));

        tracing::info!("Waiting for 2 blocks to get correct initial nonce");
        sleep(Duration::from_millis(BLOCK_TIME_MS * 2)).await;

        while let Some(requests) = self.receiver.recv().await {
            tokio::spawn(Self::transaction_handler(
                Arc::clone(&self.chain_client),
                self.platform_client.clone(),
                self.keypair.clone(),
                Arc::clone(&nonce_tracker),
                self.platform_url.clone(),
                self.platform_token.clone(),
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

mod fuel_tank {
    use sp_core::sr25519::Signature;
    use sp_core::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
    use subxt::utils::{AccountId32, MultiAddress};

    type BlockNumber = u32;

    #[derive(Clone, Eq, Encode, Decode, PartialEq, Debug, DecodeWithMemTracking, MaxEncodedLen)]
    pub struct CallIndex {
        pub pallet_index: u8,
        pub extrinsic_index: u8,
    }

    #[derive(Clone, Eq, Encode, Decode, PartialEq, Debug)]
    pub struct DispatchTx {
        pub call_index: CallIndex,
        pub tank_id: MultiAddress<AccountId32, u32>,
        pub rule_set_id: u32,
        pub inner_call_index: CallIndex,
        pub inner_call: Vec<u8>,
    }

    #[derive(Clone, Eq, Encode, Decode, PartialEq, Debug, DecodeWithMemTracking, MaxEncodedLen)]
    pub struct ExpirableSignature {
        /// The actual signature data
        pub signature: Signature,
        /// The block number at which this signature expires
        pub expiry_block: BlockNumber,
    }

    #[derive(
        Clone, Eq, PartialEq, Encode, Decode, MaxEncodedLen, DecodeWithMemTracking, Default,
    )]
    /// Settings for a dispatch call
    pub struct DispatchSettings {
        /// Dispatch from the `None` origin
        pub use_none_origin: bool,
        /// Pay remaining fee for transaction if the fuel tank does not have enough funds
        pub pays_remaining_fee: bool,
        /// The signature for evaluating along with expiry block
        /// [`RequireSignatureRule`](crate::RequireSignatureRule)
        pub signature: Option<ExpirableSignature>,
    }
}
