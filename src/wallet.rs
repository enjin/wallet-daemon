use crate::graphql::{
    get_pending_wallets, set_wallet_account, GetPendingWallets, SetWalletAccount,
};
use crate::platform_client;
use graphql_client::GraphQLQuery;
use log::trace;
use reqwest::{Client, Response};
use std::time::Duration;
use subxt_signer::sr25519::Keypair;
use subxt_signer::DeriveJunction;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::interval;

const ACCOUNT_POLLER_MS: u64 = 6000;
const ACCOUNT_PAGE_SIZE: i64 = 200;

#[derive(Clone)]
pub struct DeriveWalletRequest {
    request_id: i64,
    external_id: String,
    network: String,
    managed: bool,
}

impl TryFrom<get_pending_wallets::GetPendingWalletsGetPendingWalletsEdges> for DeriveWalletRequest {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn try_from(
        edge: get_pending_wallets::GetPendingWalletsGetPendingWalletsEdges,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            external_id: edge.node.external_id.ok_or("No external id")?,
            request_id: edge.node.id,
            network: edge.node.network,
            managed: edge.node.managed,
        })
    }
}

#[derive(Debug)]
pub struct DeriveWalletJob {
    client: Client,
    keypair: Keypair,
    sender: Sender<Vec<DeriveWalletRequest>>,
    platform_url: String,
    platform_token: String,
}

impl DeriveWalletJob {
    pub fn new(
        client: Client,
        keypair: Keypair,
        sender: Sender<Vec<DeriveWalletRequest>>,
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
        keypair: Keypair,
        platform_url: String,
        platform_token: String,
    ) -> (DeriveWalletJob, DeriveWalletProcessor) {
        let (sender, receiver) = tokio::sync::mpsc::channel(50_000);

        (
            DeriveWalletJob::new(
                Client::new(),
                keypair.clone(),
                sender,
                platform_url.clone(),
                platform_token.clone(),
            ),
            DeriveWalletProcessor::new(
                Client::new(),
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
        let mut interval = interval(Duration::from_millis(ACCOUNT_POLLER_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            match self.get_pending_wallets().await {
                Ok(derive_wallet_reqs) => {
                    if let Err(e) = self.sender.try_send(derive_wallet_reqs) {
                        tracing::info!("Error sending derive wallet requests: {:?}", e);
                    }
                }
                Err(e) => {
                    if e.to_string() == "Empty response body" {
                        tracing::info!("No pending wallets");
                    } else {
                        tracing::info!("Error: {:?}", e);
                    }
                }
            }
        }
    }

    async fn get_pending_wallets(
        &self,
    ) -> Result<Vec<DeriveWalletRequest>, Box<dyn std::error::Error + Send + Sync>> {
        let res = GetPendingWallets::build_query(get_pending_wallets::Variables {
            after: None,
            first: Some(ACCOUNT_PAGE_SIZE),
        });

        let res = self
            .client
            .post(&self.platform_url)
            .header("Authorization", &self.platform_token)
            .json(&res)
            .send()
            .await?;

        self.extract_wallet_requests(res).await
    }

    async fn extract_wallet_requests(
        &self,
        pending_wallets_res: Response,
    ) -> Result<Vec<DeriveWalletRequest>, Box<dyn std::error::Error + Send + Sync>> {
        let response_body: graphql_client::Response<get_pending_wallets::ResponseData> =
            pending_wallets_res.json().await?;
        let response_data = response_body.data.ok_or("No data in response")?;
        let derive_wallets_req = response_data
            .get_pending_wallets
            .ok_or("No pending wallets in response")?;

        Ok(derive_wallets_req
            .edges
            .into_iter()
            .filter_map(|p| {
                p.and_then(|p| {
                    DeriveWalletRequest::try_from(p)
                        .map_err(|e| {
                            tracing::info!("Error: {:?}", e);
                            e
                        })
                        .ok()
                })
            })
            .collect())
    }
}

pub struct DeriveWalletProcessor {
    client: Client,
    keypair: Keypair,
    receiver: Receiver<Vec<DeriveWalletRequest>>,
    platform_url: String,
    platform_token: String,
}

impl DeriveWalletProcessor {
    pub(crate) fn new(
        client: Client,
        keypair: Keypair,
        receiver: Receiver<Vec<DeriveWalletRequest>>,
        platform_url: String,
        platform_token: String,
    ) -> Self {
        Self {
            client,
            keypair,
            receiver,
            platform_url,
            platform_token,
        }
    }

    async fn derive_wallet(
        client: Client,
        keypair: Keypair,
        platform_url: String,
        platform_token: String,
        DeriveWalletRequest {
            request_id,
            external_id,
            network: _,
            managed: _,
        }: DeriveWalletRequest,
    ) {
        let derived_pair = keypair.derive([DeriveJunction::soft(external_id.clone())]);
        let derived_key = hex::encode(derived_pair.public_key().0);
        platform_client::set_wallet_account(
            client,
            platform_url,
            platform_token,
            request_id,
            external_id,
            format!("0x{derived_key}"),
        )
        .await;
    }

    async fn launch_job_scheduler(mut self) {
        while let Some(requests) = self.receiver.recv().await {
            for request in requests {
                tokio::spawn(Self::derive_wallet(
                    self.client.clone(),
                    self.keypair.clone(),
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
