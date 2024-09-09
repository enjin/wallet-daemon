#![allow(dead_code)]
#![allow(unused)]

use crate::graphql::{mark_and_list_pending_transactions, MarkAndListPendingTransactions};
use crate::platform_client::{update_transaction, PlatformExponentialBuilder};
use crate::{platform_client, BlockSubscription};
use autoincrement::prelude::*;
use autoincrement::AsyncIncrement;
use backon::{BlockingRetryable, Retryable};
use graphql_client::GraphQLQuery;
use lru::LruCache;
use reqwest::{Client, Response};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use backoff::exponential::ExponentialBackoff;
use backoff::SystemClock;
use subxt::backend::rpc::RpcClient;
use subxt::config::substrate::{BlakeTwo256, SubstrateHeader};
use subxt::config::DefaultExtrinsicParamsBuilder as Params;
use subxt::tx::Signer;
use subxt::{tx::TxStatus, OnlineClient, PolkadotConfig};
use subxt::ext::codec::Encode;
use subxt_signer::sr25519::Keypair;
use subxt_signer::DeriveJunction;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};

const NO_TRANSACTIONS_MSG: &str = "No transactions present in the body";
const BLOCK_TIME_MS: u64 = 12000;
const TRANSACTION_POLLER_MS: u64 = 6000;
const TRANSACTION_PAGE_SIZE: i64 = 5;

struct Wrapper(Vec<u8>);
impl subxt::tx::Payload for Wrapper {
    fn encode_call_data_to(
        &self,
        _metadata: &subxt::Metadata,
        out: &mut Vec<u8>,
    ) -> Result<(), subxt::ext::subxt_core::Error> {
        Ok(out.extend_from_slice(&self.0))
    }
}

#[derive(Clone)]
pub struct TransactionRequest {
    request_id: i64,
    external_id: Option<String>,
    network: String,
    payload: Vec<u8>,
}

impl TryFrom<mark_and_list_pending_transactions::MarkAndListPendingTransactionsMarkAndListPendingTransactionsEdges> for TransactionRequest {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn try_from(edge: mark_and_list_pending_transactions::MarkAndListPendingTransactionsMarkAndListPendingTransactionsEdges) -> Result<Self, Self::Error> {
        tracing::info!("{:?}", edge);
        let external_id = edge.node.wallet.as_ref().and_then(|w| w.external_id.clone());

        Ok(Self {
            external_id,
            request_id: edge.node.id,
            network: edge.node.network,
            payload: hex::decode(edge.node.encoded_data.split('x').nth(1).unwrap())?,
        })
    }
}

#[derive(Debug)]
pub struct TransactionJob {
    client: Client,
    keypair: Keypair,
    sender: Sender<Vec<TransactionRequest>>,
    platform_url: String,
    platform_token: String,
}

impl TransactionJob {
    pub fn new(
        client: Client,
        keypair: Keypair,
        sender: Sender<Vec<TransactionRequest>>,
        platform_url: String,
        platform_token: String,
    ) -> Self {
        Self {
            client,
            keypair,
            sender,
            platform_url,
            platform_token,
        }
    }

    pub fn create_job(
        rpc: Arc<OnlineClient<PolkadotConfig>>,
        block_sub: Arc<BlockSubscription>,
        keypair: Keypair,
        platform_url: String,
        platform_token: String,
    ) -> (TransactionJob, TransactionProcessor) {
        let (sender, receiver) = tokio::sync::mpsc::channel(50_000);

        (
            TransactionJob::new(
                Client::new(),
                keypair.clone(),
                sender,
                platform_url.clone(),
                platform_token.clone(),
            ),
            TransactionProcessor::new(
                rpc,
                Client::new(),
                block_sub,
                keypair,
                receiver,
                platform_url,
                platform_token,
            ),
        )
    }

    pub fn start(self) {
        tokio::spawn(async move {
            self.start_polling().await;
        });
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
                        tracing::info!("MarkAndListPendingTransactions: {}", NO_TRANSACTIONS_MSG);
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
        let res = MarkAndListPendingTransactions::build_query(
            mark_and_list_pending_transactions::Variables {
                network: None,
                after: None,
                first: Some(TRANSACTION_PAGE_SIZE),
                mark_as_processing: Some(true),
            },
        );

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
        let response_body: graphql_client::Response<
            mark_and_list_pending_transactions::ResponseData,
        > = transactions_res.json().await?;

        let response_data = response_body.data.ok_or(NO_TRANSACTIONS_MSG)?;
        let transactions_req = response_data
            .mark_and_list_pending_transactions
            .ok_or(NO_TRANSACTIONS_MSG)?
            .edges;

        if (transactions_req.is_empty()) {
            return Err(NO_TRANSACTIONS_MSG.into());
        }

        Ok(transactions_req
            .into_iter()
            .filter_map(|p| {
                p.and_then(|p| {
                    TransactionRequest::try_from(p)
                        .map_err(|e| {
                            tracing::error!("Error creating TransactionRequest: {:?}", e);
                            e
                        })
                        .ok()
                })
            })
            .collect())
    }
}
//
// #[derive(AsyncIncremental, PartialEq, Eq, Debug)]
// struct Nonce(u64);
//
// struct EnjinWallet {
//     nonce: Nonce,
//     players_nonce: Mutex<LruCache<DeriveJunction, Nonce>>,
// }

pub struct TransactionProcessor {
    chain_client: Arc<OnlineClient<PolkadotConfig>>,
    platform_client: Client,
    block_sub: Arc<BlockSubscription>,
    keypair: Keypair,
    receiver: Receiver<Vec<TransactionRequest>>,
    platform_url: String,
    platform_token: String,
}

impl TransactionProcessor {
    pub(crate) fn new(
        rpc: Arc<OnlineClient<PolkadotConfig>>,
        client: Client,
        block_sub: Arc<BlockSubscription>,
        keypair: Keypair,
        receiver: Receiver<Vec<TransactionRequest>>,
        platform_url: String,
        platform_token: String,
    ) -> Self {
        Self {
            chain_client: rpc,
            platform_client: client,
            block_sub,
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
        nonce_tracker: Arc<Mutex<LruCache<String, u64>>>,
        request_id: i64,
        payload: Vec<u8>,
        block_header: SubstrateHeader<u32, BlakeTwo256>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // Let's correct the nonce here
        let public_key = hex::encode(&keypair.public_key().0);
        let chain_nonce = chain_client.tx().account_nonce(&keypair.public_key().into()).await.unwrap();
        let correct_nonce: u64;
        {
            let mut tracker = nonce_tracker.lock().unwrap();

            let latest_nonce = tracker.get(&public_key).unwrap_or(&0u64);
            correct_nonce = *latest_nonce.max(&chain_nonce);

            let acc_format = trim_account(public_key.clone());
            tracing::warn!("Acc: {acc_format} - Using nonce: {correct_nonce:?} - Cached nonce: {latest_nonce:?} - Metadata nonce: {chain_nonce:?} - Next nonce: {:?}", correct_nonce + 1);

            tracker.put(public_key.clone(), correct_nonce + 1);
        }


        let params = Params::new().nonce(correct_nonce).mortal(&block_header, 16).build();

        // We probably need to try to create the tx (to check if it is valid before grabbing a nonce for it
        let mut transaction = chain_client
            .tx()
            .create_signed(&Wrapper(payload), &keypair, params)
            .await?
            .submit_and_watch()
            .await?;

        // let encoded_tx = hex::encode(&transaction.);
        // let mut sub = tx.submit_and_watch().await?;

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
                    // tracing::error!("Full transaction: {}", encoded_tx);
                }
                TxStatus::Broadcasted { num_peers: _ } => {
                    tracing::info!("Transaction #{} has been BROADCASTED", request_id);
                    let tx_hash = format!("0x{}", hex::encode(transaction.extrinsic_hash().0));

                    platform_client::update_transaction(
                        platform_client.clone(),
                        platform_url.clone(),
                        platform_token.clone(),
                        platform_client::Transaction {
                            id: request_id,
                            state: "BROADCAST".to_string(),
                            hash: Some(tx_hash),
                            signer: None,
                            signed_at: Some(block_header.number as i64),
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
                    return Ok(hex::encode(block.extrinsic_hash().0));
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

        // TODO: There is a bug in the platform where we can pass the hash twice
        // let hash = format!("0x{}", hex::encode(transaction.extrinsic_hash().0));
        Err(format!("Transaction #{} could not be signed or sent", request_id).into())
    }

    async fn transaction_handler(
        chain_client: Arc<OnlineClient<PolkadotConfig>>,
        platform_client: Client,
        block_subscription: Arc<BlockSubscription>,
        keypair: Keypair,
        nonce_tracker: Arc<Mutex<LruCache<String, u64>>>,
        platform_url: String,
        platform_token: String,
        TransactionRequest {
            request_id,
            external_id,
            network,
            payload,
        }: TransactionRequest,
    ) {
        let signer = if external_id.is_some() {
            keypair.derive([DeriveJunction::soft(external_id)])
        } else {
            keypair
        };

        let block_header = block_subscription.get_block_header();
        // tracing::info!("Pulling nonce {} for transaction {}", value, request_id);

        let res = backoff::future::retry(Self::default_backoff(), || async {
            match Self::submit_and_watch(
                platform_client.clone(),
                platform_url.clone(),
                platform_token.clone(),
                Arc::clone(&chain_client),
                signer.clone(),
                Arc::clone(&nonce_tracker),
                request_id,
                payload.clone(),
                block_header.clone(),
            )
            .await
            {
                Ok(hash) => Ok(hash),
                Err(e) => {
                    // Few possible errors
                    // ServerError(1010) - Invalid Transaction - Transaction is outdated
                    // ServerError(1012) - Transaction is temporally banned
                    // ServerError(1013) - Transaction already imported
                    // ServerError(1014) - Priority is too low
                    // We will reset the nonce if any error occurs
                    nonce_tracker.lock().unwrap().put(hex::encode(signer.public_key().0), 0);

                    tracing::info!("Resetting cached nonce from {} to 0", hex::encode(signer.public_key().0));
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
            Ok(hash) => {
                let trimmed_hash = trim_account(hash.clone());
                let trimmed_account = trim_account(account.clone());

                tracing::info!(
                    "Transaction #{} hash {} signed with account {} setting it to EXECUTED",
                    request_id,
                    trimmed_hash,
                    trimmed_account
                );

                platform_client::update_transaction(
                    platform_client.clone(),
                    platform_url.clone(),
                    platform_token.clone(),
                    platform_client::Transaction {
                        id: request_id,
                        state: "EXECUTED".to_string(),
                        hash: Some(format!("0x{}", hash)),
                        signer: Some(account),
                        signed_at: Some(block_header.number as i64),
                    },
                )
                .await;
            }
            Err(e) => {
                tracing::error!(
                    "Transaction #{} failed to sign with account {} setting it to ABANDONED",
                    request_id,
                    trim_account(account.clone())
                );

                platform_client::update_transaction(
                    platform_client.clone(),
                    platform_url.clone(),
                    platform_token.clone(),
                    platform_client::Transaction {
                        id: request_id,
                        state: "ABANDONED".to_string(),
                        hash: None,
                        signer: Some(account),
                        signed_at: None,
                    },
                )
                .await;
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

    async fn get_initial_nonce(&self) -> u64 {
        loop {
            tracing::info!("Waiting for 2 blocks to get the initial nonce");
            sleep(Duration::from_millis(BLOCK_TIME_MS)).await;

            match self
                .chain_client
                .tx()
                .account_nonce(&self.keypair.public_key().into())
                .await
            {
                Ok(nonce) => return nonce,
                Err(e) => tracing::error!("Error getting initial nonce, retrying: {:?}", e),
            }
        }
    }
    async fn launch_job_scheduler(mut self) {
        let nonce_tracker: Arc<Mutex<LruCache<String, u64>>> = Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(1_000).unwrap())));

        tracing::info!("Waiting for 2 blocks to get correct initial nonce");
        sleep(Duration::from_millis(BLOCK_TIME_MS * 2)).await;

        // tracing::info!(
        //     "Setting initial nonce to {} for account {}",
        //     initial_nonce,
        //     trim_account(hex::encode(self.keypair.public_key().0))
        // );

        while let Some(requests) = self.receiver.recv().await {
            for request in requests {
                if request.external_id.is_some() {
                    tracing::info!("Received transaction request: #{} for external_id {} but we are ignoring it for now", request.request_id, request.external_id.unwrap());
                    continue;
                }

                tracing::info!("Received transaction request: #{}", request.request_id);
                tokio::spawn(Self::transaction_handler(
                    Arc::clone(&self.chain_client),
                    self.platform_client.clone(),
                    self.block_sub.clone(),
                    self.keypair.clone(),
                    Arc::clone(&nonce_tracker),
                    self.platform_url.clone(),
                    self.platform_token.clone(),
                    request,
                ));
            }
        }
    }

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(self.launch_job_scheduler())
    }
}

fn trim_account(account: String) -> String {
    format!("0x{}...{}", &account[..4], &account[60..])
}
