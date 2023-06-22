//! Module for connection-related traits

use crate::UncheckedExtrinsic;
use async_trait::async_trait;
use subxt::Client;
use thiserror::Error;

/// Result from submitting a transaction
pub type ConnectionResult<T> = Result<T, ConnectionError>;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error("Error during connection with node")]
    ConnectionError(#[from] subxt::BasicError),
    #[error("No connection")]
    NoConnection,
}

/// Trait representing a live connection to a chain.
#[async_trait]
pub trait ChainConnection {
    /// Type of transaction that will be sent over.
    type Transaction;

    /// Hash representing where the transaction was included.
    type Hash;

    /// Submit a transaction to the connected chain.
    async fn submit_transaction(&self, tx: Self::Transaction) -> ConnectionResult<Self::Hash>;
}

#[async_trait]
impl<T> ChainConnection for Client<T>
where
    T: subxt::Config + std::marker::Sync + std::marker::Send,
    <T as subxt::Config>::Address: Send,
{
    type Transaction = UncheckedExtrinsic<T>;

    type Hash = <T as subxt::Config>::Hash;

    // We don't test connections
    #[cfg(not(tarpaulin_include))]
    async fn submit_transaction(&self, tx: Self::Transaction) -> ConnectionResult<Self::Hash> {
        Ok(self.rpc().submit_extrinsic(tx).await?)
    }
}
