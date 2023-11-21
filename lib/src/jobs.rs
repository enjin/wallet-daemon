use crate::client_connection::RequestExecutor;
///! Module with background poll and signing jobs.
///!
///! These will run in the background and do basically all the important stuff.
///! All requests are re-tried through exponential backoff.
use crate::config_loader::{ChainConnector, PairSig};
use crate::types::UncheckedExtrinsic;
use crate::wallet_trait::Wallet;
use crate::wallet_trait::{ContextDataProvider, EfinityContextData, KeyDerive};
use crate::EfinityWallet;
use crate::{
    chain_connection::{ChainConnection as ChainConn, ConnectionError, ConnectionResult},
    config_loader::WalletConnectionPair,
    wallet_trait::WalletError,
};
use graphql_client::GraphQLQuery;
use reqwest::Request;
use reqwest::Response;
use serde::Deserialize;
use serde::Serialize;
use sp_application_crypto::{sr25519::Pair as Sr25519Pair, Pair, Ss58Codec};
use sp_core::crypto::Ss58AddressFormat;
use std::error::Error;
use std::fmt::{Debug, Display};
use std::time::Duration;
use std::{convert::TryFrom, sync::Arc};
use subxt::bitvec::macros::internal::funty::Fundamental;
use subxt::sp_runtime::traits::{IdentifyAccount, Verify};
use tokio::time::interval;
use tokio::{
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};
use tracing::{instrument, Level};

#[derive(Debug)]
pub struct PollWalletJob<Client> {
    client: Arc<reqwest::Client>,
    client_executor: Arc<Client>,
    delay: Duration,
    wallet: Sender<Vec<DeriveWalletRequest>>,
    url: Arc<String>,
    token: Arc<String>,
    limit: i64,
}

pub async fn submit_abandoned_state<Client>(
    account_id: Option<String>,
    block: i64,
    request_id: i64,
    url: Arc<String>,
    token: Arc<String>,
    client: Arc<reqwest::Client>,
    client_executor: Arc<Client>,
) where
    Client: RequestExecutor + Debug + Send + Sync + 'static,
{
    let setting = backoff::ExponentialBackoffBuilder::new()
        .with_initial_interval(std::time::Duration::from_secs(12))
        .with_randomization_factor(0.2)
        .with_multiplier(2.0)
        .with_max_elapsed_time(Some(std::time::Duration::from_secs(120)))
        .build();

    let request_body = UpdateTransaction::build_query(update_transaction::Variables {
        state: Some(update_transaction::TransactionState::ABANDONED),
        transaction_hash: None,
        signed_at_block: Some(block),
        signing_account: account_id,
        id: request_id,
    });
    let result = backoff::future::retry(setting, || async {
        client_executor
            .execute(
                client
                    .post(&*url)
                    .header("Authorization", &*token)
                    .json(&request_body)
                    .build()?,
            )
            .await
            .map_err(backoff::Error::transient)
    })
    .await;

    if let Ok(result) = result {
        match result
            .json::<graphql_client::Response<update_transaction::ResponseData>>()
            .await
        {
            Ok(response_body) => {
                tracing::trace!(
                    "Set transaction id {:?} as ABANDONED and got {:?}",
                    request_id,
                    response_body
                )
            }
            Err(error) => {
                tracing::error!(
                    "(CRITICAL) Error decoding body {:?} of response to submitted state",
                    error
                )
            }
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn create_wallet_job_pair<T>(
    graphql_endpoint: String,
    token: String,
    delay: Duration,
    wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnector<T>, ChainConnector<T>>>,
    page_size: i64,
) -> (
    PollWalletJob<reqwest::Client>,
    DeriveWalletProcessor<T, reqwest::Client, ChainConnector<T>, ChainConnector<T>>,
)
    where
        T: subxt::Config<AccountId = <<<T as subxt::Config>::Signature as Verify>::Signer as IdentifyAccount>::AccountId> + Send + Sync,
        <T as subxt::Config>::AccountId: Display + Sync + Clone + 'static,
        <T as subxt::Config>::Address: std::fmt::Debug + Send,
        T::Extrinsic: Send + Sync,
        T::BlockNumber: Into<u64>,
        T::Address: From<T::AccountId>,
        T::Signature: From<sp_core::sr25519::Signature> + subxt::sp_runtime::traits::Verify,
        <T::Signature as Verify>::Signer: From<sp_core::sr25519::Public>,
{
    let client = Arc::new(reqwest::Client::new());
    create_wallet_job_pair_with_executor(
        graphql_endpoint,
        token,
        delay,
        wallet_connection_pair,
        page_size,
        client.clone(),
        client,
    )
}

pub(crate) fn create_wallet_job_pair_with_executor<T, Client, ChainConnection, ContextProvider>(
    graphql_endpoint: String,
    token: String,
    delay: Duration,
    wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnection, ContextProvider>>,
    page_size: i64,
    client: Arc<reqwest::Client>,
    client_executor: Arc<Client>,
) -> (
    PollWalletJob<Client>,
    DeriveWalletProcessor<T, Client, ChainConnection, ContextProvider>,
)
    where
        T: subxt::Config<AccountId = <<<T as subxt::Config>::Signature as Verify>::Signer as IdentifyAccount>::AccountId>,
        T: Send + Sync,
        <T as subxt::Config>::AccountId: Display + Sync + Clone + 'static,
        <T as subxt::Config>::Address: std::fmt::Debug + Send,
        T::Extrinsic: Send + Sync,
        T::BlockNumber: Into<u64>,
        T::Address: From<T::AccountId>,
        T::Signature: From<sp_core::sr25519::Signature> + subxt::sp_runtime::traits::Verify,
        <T::Signature as Verify>::Signer: From<sp_core::sr25519::Public>,
        Client: RequestExecutor + Debug + Send + Sync + 'static,
        ContextProvider: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        >
        + Send
        + Sync
        + 'static
        + Debug,
        ChainConnection: ChainConn<Hash = T::Hash, Transaction = UncheckedExtrinsic<T>>
        + Send
        + Sync
        + Debug
        + 'static,
{
    let (wallet, rx) = tokio::sync::mpsc::channel(50_000);
    let graphql_endpoint = Arc::new(graphql_endpoint);
    let token = Arc::new(token);
    let poll_wallet_job = PollWalletJob::new(
        graphql_endpoint.clone(),
        token.clone(),
        delay,
        wallet,
        client.clone(),
        client_executor.clone(),
        page_size,
    );

    let derive_wallet_processor =
        DeriveWalletProcessor::<T, Client, ChainConnection, ContextProvider>::new(
            rx,
            wallet_connection_pair,
            graphql_endpoint,
            token,
            client,
            client_executor,
        );

    (poll_wallet_job, derive_wallet_processor)
}

impl<Client> PollWalletJob<Client>
where
    Client: RequestExecutor + Debug + Send + Sync + 'static,
{
    #[cfg(not(tarpaulin_include))]
    fn build_request(&self, limit: Option<i64>) -> Request {
        let request_body = GetPendingWallets::build_query(get_pending_wallets::Variables {
            after: None,
            first: limit,
        });

        self.client
            .post(&*self.url)
            .header("Authorization", &*self.token)
            .json(&request_body)
            .build()
            .expect("The request for the graphql API should never be a stream")
    }

    pub fn new(
        url: Arc<String>,
        token: Arc<String>,
        delay: Duration,
        wallet: Sender<Vec<DeriveWalletRequest>>,
        client: Arc<reqwest::Client>,
        client_executor: Arc<Client>,
        limit: i64,
    ) -> Self {
        Self {
            client,
            client_executor,
            delay,
            wallet,
            url,
            token,
            limit,
        }
    }

    #[cfg(not(tarpaulin_include))]
    async fn extract_derive_wallet_requests(
        &self,
        pending_derive_wallet_requests: Response,
    ) -> Result<Vec<DeriveWalletRequest>, Box<dyn Error>> {
        let response_body: graphql_client::Response<get_pending_wallets::ResponseData> =
            pending_derive_wallet_requests.json().await?;
        tracing::trace!("Got request: {:#?}", response_body);
        let response_data = response_body.data.ok_or("Empty response body")?;

        let pending_derive_wallet_requests = response_data
            .get_pending_wallets
            .ok_or("Empty derive wallets")?;

        tracing::trace!(
            "Pending derive wallets requests: {:#?}",
            pending_derive_wallet_requests
        );

        Ok(pending_derive_wallet_requests
            .edges
            .into_iter()
            .filter_map(|p| match p {
                Some(p) => DeriveWalletRequest::try_from(p)
                    .map_err(|e| {
                        tracing::error!("Error converting to derive wallet request {:?}", e);
                        e
                    })
                    .ok(),
                None => None,
            })
            .collect())
    }

    #[cfg(not(tarpaulin_include))]
    async fn get_derive_wallet_requests(&self) -> Result<Vec<DeriveWalletRequest>, Box<dyn Error>> {
        let res = self
            .client_executor
            .execute(self.build_request(Some(self.limit)))
            .await?;
        tracing::trace!("Response: {res:?}");

        self.extract_derive_wallet_requests(res).await
    }

    #[cfg(not(tarpaulin_include))]
    #[instrument(level = Level::DEBUG)]
    async fn poll_requests(self) {
        let mut interval = interval(self.delay);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match self.get_derive_wallet_requests().await {
                Ok(derive_wallet_requests) => {
                    if let Err(e) = self.wallet.try_send(derive_wallet_requests) {
                        tracing::error!("Scheduler can't receive requests: {}", e);
                    }
                }
                Err(e) => {
                    if e.to_string() == "Empty response body" {
                        tracing::warn!("No wallets to derive")
                    } else {
                        tracing::warn!("Error while polling: {}", e)
                    }
                }
            };
        }
    }

    pub fn start_job(self) {
        tokio::spawn(self.poll_requests());
    }
}

/// Job polling graphql endpoint.
///
/// This job will continually poll the endpoint(It will be given when creating the job pair) and we enqueue any transaction request queried.
///
/// The transactions are send to the [SignProcessor] through a Channel.
#[derive(Debug)]
pub struct PollJob<Client> {
    /// Reqwest client that will be used to build request
    client: Arc<reqwest::Client>,
    /// Client to execute requests, it's separated for unit tests to be able to provide a mock
    client_executor: Arc<Client>,
    /// Delay between polling cycles
    delay: Duration,
    /// Sender that will be used to output the responses from graphql API
    tx: Sender<Vec<SignRequest>>,
    /// Url for the wallet daemon
    url: Arc<String>,
    /// Static token for authorization
    token: Arc<String>,
    /// Page size(pagination)
    limit: i64,
}

/// Creates a pair of `(PollJob, SignProcessor)` communicated through a channel.
#[allow(clippy::type_complexity)]
pub fn create_job_pair<T>(
    graphql_endpoint: String,
    token: String,
    delay: Duration,
    wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnector<T>, ChainConnector<T>>>,
    page_size: i64,
) -> (
    PollJob<reqwest::Client>,
    SignProcessor<T, reqwest::Client, ChainConnector<T>, ChainConnector<T>>,
)
    where
        T: subxt::Config<AccountId = <<<T as subxt::Config>::Signature as Verify>::Signer as IdentifyAccount>::AccountId> + Send + Sync,
        <T as subxt::Config>::AccountId: Display + Sync + Clone + 'static,
        <T as subxt::Config>::Address: std::fmt::Debug + Send,
        T::Extrinsic: Send + Sync,
        T::BlockNumber: Into<u64>,
        T::Address: From<T::AccountId>,
        T::Signature: From<sp_core::sr25519::Signature> + subxt::sp_runtime::traits::Verify,
        <T::Signature as Verify>::Signer: From<sp_core::sr25519::Public>,
{
    let client = Arc::new(reqwest::Client::new());
    create_job_pair_with_executor(
        graphql_endpoint,
        token,
        delay,
        wallet_connection_pair,
        page_size,
        client.clone(),
        client,
    )
}

pub(crate) fn create_job_pair_with_executor<T, Client, ChainConnection, ContextProvider>(
    graphql_endpoint: String,
    token: String,
    delay: Duration,
    wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnection, ContextProvider>>,
    page_size: i64,
    client: Arc<reqwest::Client>,
    client_executor: Arc<Client>,
) -> (
    PollJob<Client>,
    SignProcessor<T, Client, ChainConnection, ContextProvider>,
)
    where
        T: subxt::Config<AccountId = <<<T as subxt::Config>::Signature as Verify>::Signer as IdentifyAccount>::AccountId>,
        T: Send + Sync,
        <T as subxt::Config>::AccountId: Display + Sync + Clone + 'static,
        <T as subxt::Config>::Address: std::fmt::Debug + Send,
        T::Extrinsic: Send + Sync,
        T::BlockNumber: Into<u64>,
        T::Address: From<T::AccountId>,
        T::Signature: From<sp_core::sr25519::Signature> + subxt::sp_runtime::traits::Verify,
        <T::Signature as Verify>::Signer: From<sp_core::sr25519::Public>,
        Client: RequestExecutor + Debug + Send + Sync + 'static,
        ContextProvider: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        >
        + Send
        + Sync
        + 'static
        + Debug,
        ChainConnection: ChainConn<Hash = T::Hash, Transaction = UncheckedExtrinsic<T>>
        + Send
        + Sync
        + Debug
        + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(50_000);
    let graphql_endpoint = Arc::new(graphql_endpoint);
    let token = Arc::new(token);
    let poll_job = PollJob::new(
        Arc::clone(&graphql_endpoint),
        Arc::clone(&token),
        delay,
        tx,
        client.clone(),
        client_executor.clone(),
        page_size,
    );

    let sign_processor = SignProcessor::<T, Client, ChainConnection, ContextProvider>::new(
        rx,
        wallet_connection_pair,
        graphql_endpoint,
        token,
        client,
        client_executor,
    );

    (poll_job, sign_processor)
}

impl<Client> PollJob<Client>
where
    Client: RequestExecutor + Debug + Send + Sync + 'static,
{
    #[cfg(not(tarpaulin_include))]
    fn build_request(&self, limit: Option<i64>) -> Request {
        let request_body = MarkAndListPendingTransactions::build_query(
            mark_and_list_pending_transactions::Variables {
                after: None,
                first: limit,
            },
        );

        self.client
            .post(&*self.url)
            .header("Authorization", &*self.token)
            .json(&request_body)
            .build()
            .expect("The request for the graphql API should never be a stream")
    }

    /// Create a new PollJob
    pub fn new(
        url: Arc<String>,
        token: Arc<String>,
        delay: Duration,
        tx: Sender<Vec<SignRequest>>,
        client: Arc<reqwest::Client>,
        client_executor: Arc<Client>,
        limit: i64,
    ) -> Self {
        Self {
            client,
            client_executor,
            delay,
            tx,
            url,
            token,
            limit,
        }
    }

    // Extracts the Sign Request from a polling response
    #[cfg(not(tarpaulin_include))]
    async fn extract_sign_requests(
        &self,
        pending_sign_requests: Response,
    ) -> Result<Vec<(i64, Result<SignRequest, String>)>, Box<dyn Error>> {
        let response_body: graphql_client::Response<
            mark_and_list_pending_transactions::ResponseData,
        > = pending_sign_requests.json().await?;
        tracing::trace!("Got request: {:#?}", response_body);
        let response_data = response_body.data.ok_or("Empty response body")?;

        let pending_sign_requests = response_data
            .mark_and_list_pending_transactions
            .ok_or("Empty transactions")?;

        tracing::trace!("Pending sign requests: {:#?}", pending_sign_requests);

        Ok(pending_sign_requests
            .edges
            .into_iter()
            .filter_map(|p| p)
            .map(|p| {
                (
                    p.node.id,
                    SignRequest::try_from(p).map_err(|e| e.to_string()),
                )
            })
            .collect())
    }

    // Executes the query to get the transaction request.
    #[cfg(not(tarpaulin_include))]
    async fn get_sign_requests(&self) -> Result<Vec<SignRequest>, Box<dyn Error>> {
        let res = self
            .client_executor
            .execute(self.build_request(Some(self.limit)))
            .await?;
        tracing::trace!("Response: {res:?}");

        let extracted_requests = self.extract_sign_requests(res).await?;

        for (request_id, maybe_request) in &extracted_requests {
            if let Err(e) = maybe_request {
                tracing::error!("Error converting to sign request {:?}", e);
                submit_abandoned_state(
                    None,
                    0i64,
                    request_id.clone(),
                    self.url.clone(),
                    self.token.clone(),
                    self.client.clone(),
                    self.client_executor.clone(),
                )
                .await;
            }
        }

        Ok(extracted_requests
            .into_iter()
            .filter_map(|(_, r)| r.ok())
            .collect())
    }

    // The spawned poll request task.
    #[cfg(not(tarpaulin_include))]
    #[instrument(level = Level::DEBUG)]
    async fn poll_requests(self) {
        let mut interval = interval(self.delay);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match self.get_sign_requests().await {
                Ok(sign_requests) => {
                    if let Err(e) = self.tx.try_send(sign_requests) {
                        tracing::error!("Scheduler can't receive request: {}", e);
                    }
                }
                Err(e) => {
                    if e.to_string() == "Empty response body" {
                        tracing::warn!("No transactions to sign")
                    } else {
                        tracing::warn!("Error while polling: {}", e)
                    }
                }
            };
        }
    }

    // Starts polling for requests in the background.
    pub fn start_job(self) {
        tokio::spawn(self.poll_requests());
    }
}

pub struct DeriveWalletProcessor<T, Client, ChainConnection, ContextProvider>
where
    T: subxt::Config + Send + Sync,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
    T::Address: From<T::AccountId>,
    T::Signature: From<sp_core::sr25519::Signature>,
    ContextProvider: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        >
        + Send
        + Sync
        + 'static
        + Debug,
    ChainConnection: ChainConn<Hash = T::Hash, Transaction = UncheckedExtrinsic<T>>
        + Send
        + Sync
        + Debug
        + 'static,
{
    /// Receiver for processing sign requests
    rx: Receiver<Vec<DeriveWalletRequest>>,
    /// Wallet connection pair
    wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnection, ContextProvider>>,
    /// Url for the graphql endpoint
    url: Arc<String>,
    token: Arc<String>,
    /// GraphQl Client
    client: Arc<reqwest::Client>,
    /// Executor of reqwest::Client separated for testing puposes
    executor: Arc<Client>,
}

impl<T, Client, ChainConnection, ContextProvider> Debug
    for DeriveWalletProcessor<T, Client, ChainConnection, ContextProvider>
where
    T: subxt::Config + Send + Sync,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
    T::Address: From<T::AccountId>,
    T::Signature: From<sp_core::sr25519::Signature>,
    ContextProvider: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        >
        + Send
        + Sync
        + 'static
        + Debug,
    ChainConnection: ChainConn<Hash = T::Hash, Transaction = UncheckedExtrinsic<T>>
        + Send
        + Sync
        + 'static
        + Debug,
{
    #[cfg(not(tarpaulin_include))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeriveWalletProcessor")
            .field("rx", &self.rx)
            .field("wallet_connection_pair", &self.wallet_connection_pair)
            .field("url", &self.url)
            .field("token", &self.token)
            .field("client", &self.client)
            .finish()
    }
}

impl<T, Client, ChainConnection, ContextProvider>
    DeriveWalletProcessor<T, Client, ChainConnection, ContextProvider>
where
    T: subxt::Config + Send + Sync,
    T::Extrinsic: Send + Sync,
    T::BlockNumber: Into<u64>,
    T::AccountId: Into<T::Address> + Display + Sync + Clone + 'static,
    T::Address: std::fmt::Debug + Send,
    T::AccountId: Display,
    T::Signature: Verify + From<<Sr25519Pair as Pair>::Signature>,
    <T::Signature as Verify>::Signer:
        From<<Sr25519Pair as Pair>::Public> + IdentifyAccount<AccountId = T::AccountId>,
    T::Address: From<T::AccountId>,
    Client: RequestExecutor + Debug + Send + Sync + 'static,
    ContextProvider: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        >
        + Send
        + Sync
        + 'static
        + Debug,
    ChainConnection: ChainConn<Hash = T::Hash, Transaction = UncheckedExtrinsic<T>>
        + Send
        + Sync
        + 'static
        + Debug,
{
    /// Creates a DeriveWalletProcessor
    pub(crate) fn new(
        rx: Receiver<Vec<DeriveWalletRequest>>,
        wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnection, ContextProvider>>,
        url: Arc<String>,
        token: Arc<String>,
        client: Arc<reqwest::Client>,
        executor: Arc<Client>,
    ) -> Self {
        Self {
            rx,
            wallet_connection_pair,
            url,
            token,
            client,
            executor,
        }
    }

    // Submit the tx hash to the graphql API
    // TODO: Resubmit logic(We could bubble up the errors here instead of directly logging here)
    #[cfg(not(tarpaulin_include))]
    #[instrument(level = Level::DEBUG)]
    async fn submit_wallet_account(
        request_id: i64,
        account: String,
        url: Arc<String>,
        token: Arc<String>,
        client: Arc<reqwest::Client>,
        client_executor: Arc<Client>,
    ) {
        let setting = backoff::ExponentialBackoffBuilder::new()
            .with_initial_interval(std::time::Duration::from_secs(12))
            .with_randomization_factor(0.2)
            .with_multiplier(2.0)
            .with_max_elapsed_time(Some(std::time::Duration::from_secs(120)))
            .build();

        let request_body = SetWalletAccount::build_query(set_wallet_account::Variables {
            id: request_id,
            account: account.clone(),
        });
        let res = backoff::future::retry(setting, || async {
            client_executor
                .execute(
                    client
                        .post(&*url)
                        .header("Authorization", &*token)
                        .json(&request_body)
                        .build()?,
                )
                .await
                .map_err(backoff::Error::transient)
        })
        .await;

        let res = match res {
            Ok(res) => res,
            Err(_) => return,
        };

        let res: Result<graphql_client::Response<set_wallet_account::ResponseData>, _> =
            res.json().await;
        match res {
            Ok(response_body) => {
                tracing::trace!(
                    "Submitted account {:?} and got {:?}",
                    account,
                    response_body
                )
            }
            Err(e) => tracing::error!(
                "Error decoding body {:?} of response to submitted account",
                e
            ),
        }
    }

    // Handles a single derive wallet request
    #[cfg(not(tarpaulin_include))]
    #[instrument(level = Level::DEBUG)]
    async fn derive_wallet_request_handler(
        wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnection, ContextProvider>>,
        url: Arc<String>,
        token: Arc<String>,
        client: Arc<reqwest::Client>,
        client_executor: Arc<Client>,
        DeriveWalletRequest {
            request_id,
            external_id,
            network,
            managed,
        }: DeriveWalletRequest,
    ) {
        tracing::trace!(
            "ExternalID that we should derive a wallet for: {:?}",
            external_id
        );

        let external_id = external_id.clone();
        let wallet = &wallet_connection_pair.wallet;

        tracing::debug!(
            "Deriving wallet for external_id: {external_id:?}, request_id: {request_id:?}"
        );

        let derived = wallet.derive(external_id.clone().into(), None);
        match derived {
            Err(e) => {
                tracing::error!("{:?}", e);
            }
            Ok((derived, _)) => {
                if let Ok(account_id) = derived.account_id().await {
                    tracing::trace!(
                        "Using wallet for id {} with account {}",
                        external_id,
                        account_id
                    );

                    let converted_id: sp_core::crypto::AccountId32 =
                        Ss58Codec::from_ss58check(&*account_id.to_string()).unwrap();
                    let account = converted_id.to_ss58check_with_version(
                        Ss58AddressFormat::custom(if network == "rococo" { 195 } else { 1110 }),
                    );
                    Self::submit_wallet_account(
                        request_id,
                        account,
                        url,
                        token,
                        client,
                        client_executor,
                    )
                    .await;
                }
            }
        }
    }

    #[cfg(not(tarpaulin_include))]
    #[instrument(level = Level::DEBUG)]
    async fn launch_derive_wallet_job_schedulers(mut self) {
        while let Some(requests) = self.rx.recv().await {
            for request in requests {
                let wallet_connection_pair = Arc::clone(&self.wallet_connection_pair);
                let url = self.url.clone();
                let token = self.token.clone();
                let client = Arc::clone(&self.client);
                let client_executor = Arc::clone(&self.executor);
                tokio::spawn(Self::derive_wallet_request_handler(
                    wallet_connection_pair,
                    url,
                    token,
                    client,
                    client_executor,
                    request,
                ));
            }
        }
    }

    pub fn start_job(self) -> JoinHandle<()> {
        tokio::spawn(self.launch_derive_wallet_job_schedulers())
    }
}

// Conversion from query response to DeriveWalletRequest
impl TryFrom<get_pending_wallets::GetPendingWalletsGetPendingWalletsEdges> for DeriveWalletRequest {
    type Error = Box<dyn Error>;

    #[cfg(not(tarpaulin_include))]
    fn try_from(
        get_pending_wallets_requests: get_pending_wallets::GetPendingWalletsGetPendingWalletsEdges,
    ) -> Result<Self, Self::Error> {
        let external_id = get_pending_wallets_requests
            .node
            .external_id
            .ok_or("No external id to derive the wallet")?;

        Ok(Self {
            external_id,
            request_id: get_pending_wallets_requests.node.id,
            network: get_pending_wallets_requests.node.network,
            managed: get_pending_wallets_requests.node.managed,
        })
    }
}

/// Job processing each sign request
///
/// This job receives the [SignRequest] from the [PollJob] signs the request send it and submit the tx hash.
pub struct SignProcessor<T, Client, ChainConnection, ContextProvider>
where
    T: subxt::Config + Send + Sync,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
    T::Address: From<T::AccountId>,
    T::Signature: From<sp_core::sr25519::Signature>,
    ContextProvider: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        >
        + Send
        + Sync
        + 'static
        + Debug,
    ChainConnection: ChainConn<Hash = T::Hash, Transaction = UncheckedExtrinsic<T>>
        + Send
        + Sync
        + Debug
        + 'static,
{
    /// Receiver for processing sign requests
    rx: Receiver<Vec<SignRequest>>,
    /// Wallet connection pair
    wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnection, ContextProvider>>,
    /// Url for the graphql endpoint
    url: Arc<String>,
    token: Arc<String>,
    /// GraphQl Client
    client: Arc<reqwest::Client>,
    /// Executor of reqwest::Client separated for testing puposes
    executor: Arc<Client>,
}

impl<T, Client, ChainConnection, ContextProvider> Debug
    for SignProcessor<T, Client, ChainConnection, ContextProvider>
where
    T: subxt::Config + Send + Sync,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
    T::Address: From<T::AccountId>,
    T::Signature: From<sp_core::sr25519::Signature>,
    ContextProvider: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        >
        + Send
        + Sync
        + 'static
        + Debug,
    ChainConnection: ChainConn<Hash = T::Hash, Transaction = UncheckedExtrinsic<T>>
        + Send
        + Sync
        + 'static
        + Debug,
{
    #[cfg(not(tarpaulin_include))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignProcessor")
            .field("rx", &self.rx)
            .field("wallet_connection_pair", &self.wallet_connection_pair)
            .field("url", &self.url)
            .field("client", &self.client)
            .finish()
    }
}

// Could use a macro for more flexibility
#[cfg(not(tarpaulin_include))]
fn ignore<T, E: Error>(res: Result<T, E>) {
    if let Err(err) = res {
        tracing::error!("{:?}", err);
    }
}

impl<T, Client, ChainConnection, ContextProvider>
    SignProcessor<T, Client, ChainConnection, ContextProvider>
where
    T: subxt::Config + Send + Sync,
    T::Extrinsic: Send + Sync,
    T::BlockNumber: Into<u64>,
    T::AccountId: Into<T::Address> + Display + Sync + Clone + 'static,
    T::Address: std::fmt::Debug + Send,
    T::AccountId: Display,
    T::Signature: Verify + From<<Sr25519Pair as Pair>::Signature>,
    <T::Signature as Verify>::Signer:
        From<<Sr25519Pair as Pair>::Public> + IdentifyAccount<AccountId = T::AccountId>,
    T::Address: From<T::AccountId>,
    Client: RequestExecutor + Debug + Send + Sync + 'static,
    ContextProvider: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        >
        + Send
        + Sync
        + 'static
        + Debug,
    ChainConnection: ChainConn<Hash = T::Hash, Transaction = UncheckedExtrinsic<T>>
        + Send
        + Sync
        + 'static
        + Debug,
{
    /// Creates a SignProcessor
    pub(crate) fn new(
        rx: Receiver<Vec<SignRequest>>,
        wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnection, ContextProvider>>,
        url: Arc<String>,
        token: Arc<String>,
        client: Arc<reqwest::Client>,
        executor: Arc<Client>,
    ) -> Self {
        Self {
            rx,
            wallet_connection_pair,
            url,
            token,
            client,
            executor,
        }
    }

    // Submit the tx hash to the graphql API
    // TODO: Resubmit logic(We could bubble up the errors here instead of directly logging here)
    #[cfg(not(tarpaulin_include))]
    #[instrument(level = Level::DEBUG)]
    async fn submit_tx_hash(
        account_id: Option<String>,
        tx: T::Hash,
        block: i64,
        request_id: i64,
        url: Arc<String>,
        token: Arc<String>,
        client: Arc<reqwest::Client>,
        client_executor: Arc<Client>,
    ) {
        let setting = backoff::ExponentialBackoffBuilder::new()
            .with_initial_interval(std::time::Duration::from_secs(12))
            .with_randomization_factor(0.2)
            .with_multiplier(2.0)
            .with_max_elapsed_time(Some(std::time::Duration::from_secs(120)))
            .build();
        let hash = hex::encode(tx);

        let request_body = UpdateTransaction::build_query(update_transaction::Variables {
            state: Some(update_transaction::TransactionState::BROADCAST),
            transaction_hash: Some(format!("0x{hash}")),
            signed_at_block: Some(block),
            signing_account: account_id,
            id: request_id,
        });
        let res = backoff::future::retry(setting, || async {
            client_executor
                .execute(
                    client
                        .post(&*url)
                        .header("Authorization", &*token)
                        .json(&request_body)
                        .build()?,
                )
                .await
                .map_err(backoff::Error::transient)
        })
        .await;

        let res = match res {
            Ok(res) => res,
            Err(_) => return,
        };

        let res: Result<graphql_client::Response<update_transaction::ResponseData>, _> =
            res.json().await;
        match res {
            Ok(response_body) => {
                tracing::trace!("Submitted hash {:?} and got {:?}", tx, response_body)
            }
            Err(e) => tracing::error!("Error decoding body {:?} of response to submitted hash", e),
        }
    }

    // Sign a transaction and submit the returned hash.
    #[cfg(not(tarpaulin_include))]
    async fn sign_and_submit(
        wallet: &EfinityWallet<T, PairSig<T>, ContextProvider>,
        connection: &Arc<ChainConnection>,
        transaction: Vec<u8>,
        external_id: Option<String>,
    ) -> ConnectionResult<T::Hash> {
        let signed = if let Some(external_id) = external_id {
            // This has security implications beware
            let wallet = wallet.derive(external_id.to_string().into(), None);
            match wallet {
                Err(e) => {
                    tracing::error!("{:?}", e);
                    return Err(ConnectionError::ConnectionError(subxt::BasicError::Other(
                        "Non-derivable wallet".into(),
                    )));
                }
                Ok((wallet, _)) => {
                    if let Ok(account_id) = wallet.account_id().await {
                        tracing::trace!(
                            "Using wallet for id {} with account {}",
                            external_id,
                            account_id
                        );
                    }
                    wallet.sign_transaction(transaction).await
                }
            }
        } else {
            if let Ok(account_id) = wallet.account_id().await {
                tracing::trace!("Using base wallet {}", account_id);
            }
            wallet.sign_transaction(transaction).await
        };
        tracing::trace!("Signed: {:?}", signed);
        match signed {
            Ok(tx) => connection.submit_transaction(tx).await,
            Err(e) => {
                ignore(wallet.reset_nonce().await);
                match e {
                    WalletError::Disconnected => Err(ConnectionError::NoConnection),
                    WalletError::ClientError(e) => Err(ConnectionError::ConnectionError(e)),
                }
            }
        }
    }

    // Reset wallet's nonce
    #[cfg(not(tarpaulin_include))]
    async fn reset_wallet_nonce(
        wallet: &EfinityWallet<T, PairSig<T>, ContextProvider>,
        external_id: &Option<String>,
    ) {
        if let Some(external_id) = external_id {
            // This has security implications beware
            let wallet = wallet.derive(external_id.into(), None);
            match wallet {
                Err(e) => {
                    tracing::error!(
                        "Error deriving wallet with external id {}: {:?}",
                        external_id,
                        e
                    );
                }
                Ok((wallet, _)) => {
                    ignore(wallet.reset_nonce().await);
                }
            }
        } else {
            ignore(wallet.reset_nonce().await);
        };
    }

    async fn get_account_id(
        wallet: &EfinityWallet<T, PairSig<T>, ContextProvider>,
        external_id: &Option<String>,
    ) -> Option<String> {
        if let Some(external_id) = external_id {
            // This has security implications beware
            let wallet = wallet.derive(external_id.into(), None);

            match wallet {
                Err(e) => {
                    tracing::error!(
                        "Error deriving wallet with external id {}: {:?}",
                        external_id,
                        e
                    );
                    None
                }
                Ok((wallet, _)) => Some(wallet.account_id().await.unwrap().to_string()),
            }
        } else {
            Some(wallet.account_id().await.unwrap().to_string())
        }
    }

    // Handles a single sign request
    #[cfg(not(tarpaulin_include))]
    #[instrument(level = Level::DEBUG)]
    async fn sign_request_handler(
        wallet_connection_pair: Arc<WalletConnectionPair<T, ChainConnection, ContextProvider>>,
        url: Arc<String>,
        token: Arc<String>,
        client: Arc<reqwest::Client>,
        client_executor: Arc<Client>,
        SignRequest {
            transaction,
            request_id,
            external_id,
        }: SignRequest,
    ) {
        tracing::trace!("Transaction to sign: {:?}", transaction);

        let setting = backoff::ExponentialBackoffBuilder::new()
            .with_initial_interval(std::time::Duration::from_secs(6))
            .with_randomization_factor(0.2)
            .with_multiplier(2.0)
            .with_max_elapsed_time(Some(std::time::Duration::from_secs(120)))
            .build();
        let res = backoff::future::retry(setting, || async {
            let transaction = transaction.clone();
            let wallet = &wallet_connection_pair.wallet;

            let connection = &wallet_connection_pair.connection;
            tracing::debug!("Signing for external_id: {external_id:?}, request_id: {request_id:?}");
            match Self::sign_and_submit(wallet, connection, transaction, external_id.clone()).await {
                Ok(tx_hash) => Ok(tx_hash),
                Err(ConnectionError::NoConnection) => {
                    tracing::warn!("external_id: {external_id:?}, request_id: {request_id:?}, No connection");
                    Self::reset_wallet_nonce(wallet, &external_id).await;
                    Err(backoff::Error::transient(()))
                },
                Err(ConnectionError::ConnectionError(subxt::BasicError::Rpc(e))) => {
                    tracing::warn!("external_id: {external_id:?}, request_id: {request_id:?}, Error submitting transaction {e:?}");
                    Self::reset_wallet_nonce(wallet, &external_id).await;
                    Err(backoff::Error::transient(()))
                },
                Err(ConnectionError::ConnectionError(subxt::BasicError::Invalid(e))) => {
                    tracing::warn!("external_id: {external_id:?}, request_id: {request_id:?}, Error processing transaction {e:?}");
                    Self::reset_wallet_nonce(wallet, &external_id).await;
                    Err(backoff::Error::transient(()))
                },
                _ => {
                    tracing::warn!("Failed to submit for external_id: {external_id:?}, request_id: {request_id:?}");
                    Self::reset_wallet_nonce(wallet, &external_id).await;
                    Err(backoff::Error::Permanent(()))
                }
            }
        }).await;
        tracing::debug!(
            "Signed for external_id: {external_id:?}, request_id: {request_id:?}, result: {res:?}"
        );

        let block = wallet_connection_pair
            .wallet
            .get_block_number()
            .await
            .unwrap_or_default();
        tracing::trace!("Block number: {:?}", block);

        // TODO: Check if has to derive
        let account_id = Self::get_account_id(&wallet_connection_pair.wallet, &external_id).await;

        match res {
            Ok(tx) => {
                tracing::trace!("Transaction: {:?} Signed", tx);
                Self::submit_tx_hash(
                    account_id,
                    tx,
                    block.as_i64(),
                    request_id,
                    url,
                    token,
                    client,
                    client_executor,
                )
                .await;
            }
            Err(e) => {
                tracing::error!("Error signing {:?}", e);
                submit_abandoned_state(
                    account_id,
                    block.as_i64(),
                    request_id,
                    url,
                    token,
                    client,
                    client_executor,
                )
                .await;
            }
        }
    }

    // Cycles through sign request and launch a new background task to process it
    #[cfg(not(tarpaulin_include))]
    #[instrument(level = Level::DEBUG)]
    async fn launch_sign_job_schedulers(mut self) {
        while let Some(requests) = self.rx.recv().await {
            for request in requests {
                let wallet_connection_pair = Arc::clone(&self.wallet_connection_pair);
                let url = self.url.clone();
                let token = self.token.clone();
                let client = Arc::clone(&self.client);
                let client_executor = Arc::clone(&self.executor);
                tokio::spawn(Self::sign_request_handler(
                    wallet_connection_pair,
                    url,
                    token,
                    client,
                    client_executor,
                    request,
                ));
            }
        }
    }

    /// Start the Processor
    pub fn start_job(self) -> JoinHandle<()> {
        tokio::spawn(self.launch_sign_job_schedulers())
    }
}

// Conversion from query response to SignRequest
impl TryFrom<mark_and_list_pending_transactions::MarkAndListPendingTransactionsMarkAndListPendingTransactionsEdges>
for SignRequest
{
    type Error = Box<dyn Error>;

    #[cfg(not(tarpaulin_include))]
    fn try_from(
        get_pending_sign_requests: mark_and_list_pending_transactions::MarkAndListPendingTransactionsMarkAndListPendingTransactionsEdges,
    ) -> Result<Self, Self::Error> {
        let data = get_pending_sign_requests
            .node
            .encoded_data
            .split('x')
            .nth(1)
            .ok_or("No '0x' at the beggining")?;
        let transaction = hex::decode(data).map_err(|e| {
            tracing::error!("Error decoding: {:?}", e);
            e
        })?;

        if let Some(wallet) = get_pending_sign_requests.node.wallet {
            Ok(Self {
                transaction,
                request_id: get_pending_sign_requests.node.id,
                external_id: wallet.external_id,
            })
        } else {
            Ok(Self {
                transaction,
                request_id: get_pending_sign_requests.node.id,
                external_id: None,
            })
        }
    }
}

/// A request for a wallet to be derived
#[derive(Clone)]
pub struct DeriveWalletRequest {
    /// Unique identifier for derive wallet request
    request_id: i64,
    /// The external id of a player
    external_id: String,
    /// The network which the address is going to be generated
    network: String,
    /// If this is a managed wallet
    managed: bool,
}

/// A request of a transaction to be signed
#[derive(Clone)]
pub struct SignRequest {
    /// The transaction in its decoded representation
    transaction: Vec<u8>,
    /// Unique identifier for sign request
    request_id: i64,
    /// External id of a derive wallet
    external_id: Option<String>,
}

/// Part of the GraphQL Schema that Juniper requires we define here.
#[derive(Serialize, Deserialize)]
pub struct PaginationInput {
    limit: Option<i64>,
}

/// GraphQL query for getting pending transactions
///
/// The query is actually a mutation since when we request the pending transactions they are mutated to `processing`
/// This is done to prevent double-submission of a transactionRequest.
///
/// For example, if the wallet daemon dies while a transaction was sent but the response not received
/// when it is restarted if the query wasn't mutated it would re-sign the same transaction.
///
/// ## The query (Can be found in the `query_path`)
/// ```ignore
/// mutation MarkAndListPendingTransactions($after: String, $first: Int) {
///     MarkAndListPendingTransactions(after: $after, first: $first) {
///         edges {
///             node {
///                 encodedData
///                 id
///                 wallet {
///                     externalId
///                     managed
///                 }
///             }
///         }
///     }
/// }
/// ````
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql_schemas/schema.graphql",
    query_path = "src/graphql_schemas/mark_and_list_pending_transactions.graphql",
    response_derives = "Debug"
)]
pub struct MarkAndListPendingTransactions;

/// GraphQL mutation to submit the received txHash.
///
/// ## The query
/// ```ignore
/// mutation UpdateTransaction(
///     $id: Int!
///     $signingAccount: String
///     $state: TransactionState
///     $transactionHash: String
///     $signedAtBlock: Int
/// ) {
///     UpdateTransaction(id: $id, signingAccount: $signingAccount, state: $state, transactionHash: $transactionHash, signedAtBlock: $signedAtBlock)
/// }
/// ```
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql_schemas/schema.graphql",
    query_path = "src/graphql_schemas/update_transaction.graphql",
    response_derives = "Debug"
)]
pub struct UpdateTransaction;

/// GraphQL mutation to submit the received txHash.
///
/// ## The query
/// ```ignore
/// mutation SetWalletAccount(
///     $id: Int!
///     $account: String!
/// ) {
///     SetWalletAccount(id: $id, account: $account)
/// }
/// ```
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql_schemas/schema.graphql",
    query_path = "src/graphql_schemas/set_wallet_account.graphql",
    response_derives = "Debug"
)]
pub struct SetWalletAccount;

/// GraphQL query to get the wallets that need to be derived.
///
/// ## The query
/// ```ignore
/// query GetPendingWallets($after: String, $first: Int) {
///     GetPendingWallets(after: $after, first: $first) {
///         edges {
///             node {
///                 id
///                 account {
///                     publicKey
///                     address
///                 }
///                 externalId
///                 managed
///                 network
///             }
///         }
///     }
/// }
/// ```
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql_schemas/schema.graphql",
    query_path = "src/graphql_schemas/get_pending_wallets.graphql",
    response_derives = "Debug"
)]
pub struct GetPendingWallets;
