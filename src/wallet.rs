use crate::graphql::populate_managed_wallets::PopulateManagedWalletInput;
use crate::graphql::{get_pending_managed_wallet_creations, GetPendingManagedWalletCreations};
use crate::platform_client;
use graphql_client::GraphQLQuery;
use reqwest::{Client, Response};
use std::time::Duration;
use subxt_signer::sr25519::Keypair;
use subxt_signer::DeriveJunction;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::interval;

const ACCOUNT_POLLER_MS: u64 = 6000;
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

#[derive(Debug)]
pub struct DeriveWalletJob {
    client: Client,
    sender: Sender<Vec<DeriveWalletRequest>>,
    platform_url: String,
    platform_token: String,
}

impl DeriveWalletJob {
    pub fn new(
        client: Client,
        sender: Sender<Vec<DeriveWalletRequest>>,
        platform_url: String,
        platform_token: String,
    ) -> Self {
        Self {
            client,
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

    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.start_polling().await;
        })
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
        let res = GetPendingManagedWalletCreations::build_query(
            get_pending_managed_wallet_creations::Variables {
                // TODO: get these from the config
                network: crate::NETWORK.into(),
                chain: crate::CHAIN.into(),
                limit: ACCOUNT_PAGE_SIZE,
                cursor: None,
            },
        );

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
        let response_body: graphql_client::Response<
            get_pending_managed_wallet_creations::ResponseData,
        > = pending_wallets_res.json().await?;
        let response_data = response_body.data.ok_or("No data in response")?;
        let derive_wallets_req = response_data
            .result
            .ok_or("No pending wallets in response")?;

        Ok(derive_wallets_req
            .data
            .into_iter()
            .filter_map(|p| {
                DeriveWalletRequest::try_from(p)
                    .map_err(|e| {
                        tracing::info!("Error: {:?}", e);
                        e
                    })
                    .ok()
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

    async fn derive_wallets(
        client: Client,
        keypair: Keypair,
        platform_url: String,
        platform_token: String,
        requests: Vec<DeriveWalletRequest>,
    ) {
        let wallets: Vec<_> = requests
            .into_iter()
            .map(|request| {
                let external_id = request.external_id;
                let derive_junction = match external_id.parse::<i64>() {
                    Ok(_) => DeriveJunction::soft(external_id.parse::<i64>().unwrap()),
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

        if !wallets.is_empty() {
            platform_client::populate_managed_wallets(
                client,
                platform_url,
                platform_token,
                wallets,
            )
            .await;
        }
    }

    async fn launch_job_scheduler(mut self) {
        while let Some(requests) = self.receiver.recv().await {
            tokio::spawn(Self::derive_wallets(
                self.client.clone(),
                self.keypair.clone(),
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
