mod fuel_tank;

use crate::graphql::sign_transactions::SignTransactionInput;
use crate::graphql::{get_pending_transactions, GetPendingTransactions};
use crate::subscription::Network;
use crate::transaction::fuel_tank::ExpirableSignature;
use crate::{platform_client, SubscriptionParams, DUMMY_TX_MORTALITY, TX_MORTALITY};
use graphql_client::GraphQLQuery;
use lru::LruCache;
use parity_scale_codec::Encode;
use reqwest::{Client, Response};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subxt::config::DefaultExtrinsicParamsBuilder;
use subxt::{OnlineClient, PolkadotConfig};
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

    async fn transaction_handler(
        chain_client: Arc<OnlineClient<PolkadotConfig>>,
        platform_client: Client,
        keypair: Keypair,
        nonce_tracker: Arc<Mutex<LruCache<String, u64>>>,
        platform_url: String,
        platform_token: String,
        requests: Vec<TransactionRequest>,
    ) {
        let mut inputs = Vec::with_capacity(requests.len());
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

                // remove the last byte of the payload because it is the settings param, and we are
                // replacing it
                payload.pop();

                // create message to be signed
                let Ok(message) =
                    fuel_tank::create_message(&payload, &public_key, expiration_block)
                else {
                    continue;
                };

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
        platform_client::sign_transactions(
            platform_client.clone(),
            platform_url.clone(),
            platform_token.clone(),
            inputs,
        )
        .await;
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
