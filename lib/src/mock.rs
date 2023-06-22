//! Mocking structs for unit testing.
use std::fmt::Debug;
use std::sync::Mutex;

use crate::config_loader::PairSig;
use crate::connection::chain_connection::{ChainConnection, ConnectionResult};
use crate::connection::client_connection::RequestExecutor;
use crate::connection::connection_handler::{Connector, TryConnect};
use crate::types::UncheckedExtrinsic;
use crate::wallet_trait::{ContextDataProvider, EfinityContextData, WalletResult};
use crate::EfinityWallet;
use async_trait::async_trait;
use backoff::Error;
use futures::FutureExt;
use http::response::Builder;
use reqwest::Response;
use subxt::rpc::RuntimeVersion;
use subxt::sp_runtime::traits::Zero;
use subxt::{Config, DefaultConfig};

#[derive(Default, Clone)]
pub struct MockEfinityDataProvider<T>
where
    T: subxt::Config,
{
    _phantom_data: std::marker::PhantomData<T>,
}

impl<T: subxt::Config> MockEfinityDataProvider<T> {
    pub fn new() -> Self {
        Self {
            _phantom_data: std::marker::PhantomData,
        }
    }
}

impl<T> Debug for MockEfinityDataProvider<T>
where
    T: subxt::Config,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockEfinityDataProvider")
            .field("_phantom_data", &self._phantom_data)
            .finish()
    }
}

#[async_trait]
impl<T> ContextDataProvider for MockEfinityDataProvider<T>
where
    T: subxt::Config + std::marker::Sync,
    <T as subxt::Config>::AccountId: Sync + Clone,
    T::Hash: Default,
{
    type ContextData = EfinityContextData<<T as subxt::Config>::Hash, <T as subxt::Config>::Index>;
    type AccountId = <T as subxt::Config>::AccountId;
    type Nonce = <T as subxt::Config>::Index;
    async fn get_context_data(
        &self,
        _account: &Self::AccountId,
    ) -> WalletResult<Self::ContextData> {
        Ok(EfinityContextData {
            nonce: Zero::zero(),
            block_number: 0,
            runtime_version: RuntimeVersion {
                spec_version: 0,
                transaction_version: 0,
                other: Default::default(),
            },
            genesis_hash: Default::default(),
            block_hash: Default::default(),
        })
    }
}

pub type MockEfinityWallet =
    EfinityWallet<DefaultConfig, PairSig<DefaultConfig>, MockEfinityDataProvider<DefaultConfig>>;

#[derive(Debug)]
pub struct MockChainConnection {
    received: Mutex<Vec<UncheckedExtrinsic<DefaultConfig>>>,
}

impl MockChainConnection {
    pub(crate) fn new() -> Self {
        Self {
            received: Mutex::new(vec![]),
        }
    }

    #[cfg(test)]
    pub(crate) fn get_transactions(&self) -> Vec<UncheckedExtrinsic<DefaultConfig>> {
        self.received.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChainConnection for MockChainConnection {
    type Transaction = UncheckedExtrinsic<DefaultConfig>;

    type Hash = <DefaultConfig as Config>::Hash;

    async fn submit_transaction(
        &self,
        transaction: Self::Transaction,
    ) -> ConnectionResult<Self::Hash> {
        let mut recieved = self.received.lock().unwrap();
        recieved.push(transaction);

        Ok(Default::default())
    }
}

#[derive(Debug)]
pub struct MockClient {
    submitted_hashes: Mutex<Vec<String>>,
}

#[cfg(test)]
impl MockClient {
    pub(crate) fn new() -> Self {
        Self {
            submitted_hashes: Mutex::new(vec![]),
        }
    }

    pub(crate) fn get_hashes(&self) -> Vec<String> {
        self.submitted_hashes.lock().unwrap().clone()
    }
}

#[async_trait]
impl RequestExecutor for MockClient {
    async fn execute(&self, req: reqwest::Request) -> Result<Response, reqwest::Error> {
        let res_body = std::str::from_utf8(req.body().unwrap().as_bytes().unwrap()).unwrap();
        if res_body.contains("MarkAndListPendingTransactions") {
            let body = r#"{
                "data": {
                    "MarkAndListPendingTransactions": {
                        "edges": [
                            {
                                "cursor": "1",
                                "node": {
                                    "id": 1,
                                    "encodedData": "0x2800016400000000000000016400000000000000000000000000000000",
                                    "wallet": {
                                        "externalId": null,
                                        "managed": true
                                    }
                                }
                            }
                        ],
                        "pageInfo": {
                            "startCursor": "",
                            "endCursor": "",
                            "hasNextPage": false,
                            "hasPreviousPage": false
                        },
                        "totalCount": 1
                    }
                }
            }"#;
            let response = Builder::new()
                .status(200)
                .header("content-type", "application/json")
                .body(body)
                .unwrap();
            Ok(response.into())
        } else {
            let query: serde_json::Value = serde_json::from_str(res_body).unwrap();
            let transaction_hash = query["variables"]["transactionHash"].clone();
            let mut hashes = self.submitted_hashes.lock().unwrap();
            hashes.push(transaction_hash.to_string());

            let body = r#"{"data":{"UpdateTransaction": true}}"#;
            let response = Builder::new()
                .status(200)
                .header("content-type", "application/json")
                .body(body)
                .unwrap();
            Ok(response.into())
        }
    }
}

#[async_trait]
impl ChainConnection for Connector<MockChainConnection, String, subxt::BasicError> {
    type Transaction = UncheckedExtrinsic<DefaultConfig>;

    type Hash = <DefaultConfig as subxt::Config>::Hash;

    // We don't test connections
    #[cfg(not(tarpaulin_include))]
    async fn submit_transaction(&self, tx: Self::Transaction) -> ConnectionResult<Self::Hash> {
        use crate::connection::{
            chain_connection::ConnectionError, connection_handler::RequestError,
        };

        match self
            .execute(|conn| async move { conn.submit_transaction(tx).await }.boxed())
            .await
        {
            Ok(r) => Ok(r),
            Err(RequestError::SubmitionProblem(e)) => Err(e),
            Err(RequestError::GotDisconnected(e)) => Err(e),
            Err(RequestError::NoConnection) => Err(ConnectionError::NoConnection),
        }
    }
}

#[async_trait]
impl<T> ContextDataProvider for Connector<MockEfinityDataProvider<T>, String, subxt::BasicError>
where
    T: subxt::Config + Send + Sync,
    <T as subxt::Config>::AccountId: Sync + Clone,
    T::AccountId: Clone,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
{
    type ContextData = EfinityContextData<<T as subxt::Config>::Hash, <T as subxt::Config>::Index>;
    type AccountId = <T as subxt::Config>::AccountId;
    type Nonce = <T as subxt::Config>::Index;
    // We don't test connections
    #[cfg(not(tarpaulin_include))]
    async fn get_context_data(&self, account: &Self::AccountId) -> WalletResult<Self::ContextData> {
        use crate::{connection::connection_handler::RequestError, wallet_trait::WalletError};

        let account = account.clone();
        match self
            .execute(|conn| {
                async move { ContextDataProvider::get_context_data(conn, &account).await }.boxed()
            })
            .await
        {
            Ok(r) => Ok(r),
            Err(RequestError::SubmitionProblem(e)) => Err(e),
            Err(RequestError::GotDisconnected(e)) => Err(e),
            Err(RequestError::NoConnection) => Err(WalletError::Disconnected),
        }
    }
}

#[async_trait]
impl TryConnect for MockChainConnection {
    type ConnectionParameters = String;
    type ConnectionError = subxt::BasicError;

    // We don't test connections
    #[cfg(not(tarpaulin_include))]
    async fn try_connect(
        _: Self::ConnectionParameters,
    ) -> Result<Self, Error<Self::ConnectionError>> {
        Ok(Self::new())
    }
}

#[async_trait]
impl<T: subxt::Config + Send + Sync> TryConnect for MockEfinityDataProvider<T> {
    type ConnectionParameters = String;
    type ConnectionError = subxt::BasicError;

    // We don't test connections
    #[cfg(not(tarpaulin_include))]
    async fn try_connect(
        _: Self::ConnectionParameters,
    ) -> Result<Self, Error<Self::ConnectionError>> {
        Ok(MockEfinityDataProvider::new())
    }
}
