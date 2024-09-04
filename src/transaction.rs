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
use std::time::Duration;
use subxt::backend::rpc::RpcClient;
use subxt::config::DefaultExtrinsicParamsBuilder as Params;
use subxt::tx::Signer;
use subxt::{tx::TxStatus, OnlineClient, PolkadotConfig};
use subxt_signer::sr25519::Keypair;
use subxt_signer::DeriveJunction;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::interval;

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
        rpc: OnlineClient<PolkadotConfig>,
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
                    if e.to_string() == "Empty response body" {
                        tracing::info!("No pending transactions");
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

        let response_data = response_body.data.ok_or("No data in response")?;
        let transactions_req = response_data
            .mark_and_list_pending_transactions
            .ok_or("No transactions in response")?;

        Ok(transactions_req
            .edges
            .into_iter()
            .filter_map(|p| {
                p.and_then(|p| {
                    TransactionRequest::try_from(p)
                        .map_err(|e| {
                            tracing::error!("Error response: {:?}", e);
                            e
                        })
                        .ok()
                })
            })
            .collect())
    }
}

#[derive(AsyncIncremental, PartialEq, Eq, Debug)]
struct Nonce(u64);

struct EnjinWallet {
    nonce: Nonce,
    players_nonce: Mutex<LruCache<DeriveJunction, Nonce>>,
}

pub struct TransactionProcessor {
    rpc: OnlineClient<PolkadotConfig>,
    client: Client,
    block_sub: Arc<BlockSubscription>,
    keypair: Keypair,
    receiver: Receiver<Vec<TransactionRequest>>,
    platform_url: String,
    platform_token: String,
}

impl TransactionProcessor {
    pub(crate) fn new(
        rpc: OnlineClient<PolkadotConfig>,
        client: Client,
        block_sub: Arc<BlockSubscription>,
        keypair: Keypair,
        receiver: Receiver<Vec<TransactionRequest>>,
        platform_url: String,
        platform_token: String,
    ) -> Self {
        Self {
            rpc,
            client,
            block_sub,
            keypair,
            receiver,
            platform_url,
            platform_token,
        }
    }

    async fn submit_and_watch(
        client: Client,
        platform_url: String,
        platform_token: String,
        rpc: OnlineClient<PolkadotConfig>,
        keypair: Keypair,
        nonce: u64,
        request_id: i64,
        payload: Vec<u8>,
        block_number: i64,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let params = Params::new().nonce(nonce).build();

        let mut transaction = rpc
            .tx()
            .create_signed(&Wrapper(payload), &keypair, params)
            .await?
            .submit_and_watch()
            .await?;

        // TODO: There is a bug in the platform where we can pass the hash twice
        // let hash = format!("0x{}", hex::encode(transaction.extrinsic_hash().0));

        while let Some(status) = transaction.next().await {
            match status? {
                TxStatus::Validated => {
                    let trimmed = trim_account(hex::encode(keypair.public_key().0));
                    tracing::info!(
                        "Sent transaction #{} with nonce {} signed by {}",
                        request_id,
                        nonce,
                        trimmed
                    );
                }
                TxStatus::Invalid { message } => {
                    tracing::error!("Transaction #{} is INVALID: {:?}", request_id, message);
                }
                TxStatus::Broadcasted { num_peers: _ } => {
                    tracing::info!("Transaction #{} has been BROADCASTED", request_id);
                    platform_client::update_transaction(
                        client.clone(),
                        platform_url.clone(),
                        platform_token.clone(),
                        platform_client::Transaction {
                            id: request_id,
                            state: "BROADCAST".to_string(),
                            hash: None,
                            signer: None,
                            signed_at: Some(block_number),
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

        Err("Transaction #{} could not be signed or sent".into())
    }

    async fn transaction_handler(
        rpc: OnlineClient<PolkadotConfig>,
        client: Client,
        block_sub: Arc<BlockSubscription>,
        keypair: Keypair,
        nonce: Arc<AsyncIncrement<Nonce>>,
        platform_url: String,
        platform_token: String,
        TransactionRequest {
            request_id,
            external_id,
            network,
            payload,
        }: TransactionRequest,
    ) {
        let setting = backoff::ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_secs(6))
            .with_randomization_factor(0.2)
            .with_multiplier(2.0)
            .with_max_elapsed_time(Some(Duration::from_secs(120)))
            .build();

        let signer = if external_id.is_some() {
            keypair.derive([DeriveJunction::soft(external_id)])
        } else {
            keypair
        };

        let nonce_value = nonce.pull();
        let value = nonce_value.0.clone();
        let block_number = block_sub.get_block_number() as i64;

        let res = backoff::future::retry(setting, || async {
            match Self::submit_and_watch(
                client.clone(),
                platform_url.clone(),
                platform_token.clone(),
                rpc.clone(),
                signer.clone(),
                value,
                request_id,
                payload.clone(),
                block_number.clone(),
            )
            .await
            {
                Ok(hash) => Ok(hash),
                Err(e) => {
                    tracing::error!("Error submitting transaction: {:?}", e);
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
                    client.clone(),
                    platform_url.clone(),
                    platform_token.clone(),
                    platform_client::Transaction {
                        id: request_id,
                        state: "EXECUTED".to_string(),
                        hash: Some(format!("0x{}", hash)),
                        signer: Some(account),
                        signed_at: Some(block_number),
                    },
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(
                    "Transaction #{} failed to sign with account {} setting it to ABANDONED",
                    request_id,
                    trim_account(account.clone())
                );

                platform_client::update_transaction(
                    client.clone(),
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

    async fn launch_job_scheduler(mut self) {
        // TODO: Change this as we can have many accounts that can have diff nonces
        let initial_nonce = self
            .rpc
            .tx()
            .account_nonce(&self.keypair.public_key().into())
            .await
            .unwrap();

        let nonce_tracker = Arc::new(Nonce(initial_nonce).init_from());
        tracing::info!(
            "Setting initial nonce to {} for account {}",
            initial_nonce,
            trim_account(hex::encode(self.keypair.public_key().0))
        );

        // let wallet = EnjinWallet {
        //     nonce: Nonce(initial_nonce),
        //     players_nonce: Mutex::new(LruCache::new(NonZeroUsize::new(1_000).unwrap())),
        // };

        while let Some(requests) = self.receiver.recv().await {
            for request in requests {
                tracing::info!("Received transaction request: #{}", request.request_id);
                tokio::spawn(Self::transaction_handler(
                    self.rpc.clone(),
                    self.client.clone(),
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
