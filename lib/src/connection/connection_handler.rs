//! Connection handlers wraps different connections and hides the complexities of retrying connecting and submission.
use crate::{
    chain_connection::{ChainConnection, ConnectionError, ConnectionResult},
    wallet_trait::{ContextDataProvider, EfinityContextData, WalletError, WalletResult},
    UncheckedExtrinsic,
};
use async_trait::async_trait;
use backoff::Error;
use backoff::ExponentialBackoff;
use futures::future::{BoxFuture, FutureExt};
use std::sync::Arc;
use subxt::{Client, ClientBuilder};
use thiserror::Error;
use tokio::sync::RwLock;

#[cfg(test)]
use subxt::DefaultConfig;

#[cfg(test)]
use crate::mock::MockChainConnection;

// A wrapper of a long lived connection, it can either be connected or discionnected
// The connection is used through trait implementtions in `Connector`.
#[derive(Debug)]
pub enum ConnectionWrapper<T: TryConnect<ConnectionParameters = U, ConnectionError = V>, U, V> {
    Connected(Connected<T, U>),
    Disconnected(Disconnected<U>),
}

// A connection with its parameters(They are stored to re-establish the connection if needed)
#[derive(Debug)]
pub struct Connected<T, U> {
    connection: T,
    connection_parameters: U,
}

// A "disconnected" connection.
#[derive(Debug)]
pub struct Disconnected<U> {
    connection_parameters: U,
    reconnection_in_progress: bool,
}

#[derive(Debug, Error)]
pub enum RequestError<E> {
    #[error(
        "A problem when executing a command in a connection not related to the connection itself"
    )]
    SubmitionProblem(E),
    #[error("The connection just cut off")]
    GotDisconnected(E),
    #[error("The connection was already off")]
    NoConnection,
}

impl From<ConnectionError> for Error<subxt::BasicError> {
    fn from(e: ConnectionError) -> Self {
        match e {
            ConnectionError::ConnectionError(subxt::BasicError::Rpc(e)) => {
                Self::transient(subxt::BasicError::Rpc(e))
            }
            ConnectionError::ConnectionError(e) => Self::Permanent(e),
            ConnectionError::NoConnection => {
                panic!("Should never try to execute request while disconnected")
            }
        }
    }
}

// A connector will try to connect asynchronously in a background in a new task.
// It will start disconnected and will start a task that will try to reconnect and this will happen automatically when a method is called and the connection is dead.
// (This means it doesn't poll for disconnection it detects it when a method using the connection is called and was disconnected before)
// It uses ExponentialBackoff for reconnection
#[derive(Debug)]
pub struct Connector<T: TryConnect<ConnectionParameters = U, ConnectionError = V>, U, V>(
    Arc<RwLock<ConnectionWrapper<T, U, V>>>,
);

impl<T: TryConnect<ConnectionParameters = U, ConnectionError = V>, U, V> Clone
    for Connector<T, U, V>
{
    fn clone(&self) -> Self {
        Connector(self.0.clone())
    }
}

impl<T, U, V> Connector<T, U, V>
where
    T: TryConnect<ConnectionParameters = U, ConnectionError = V> + Send + Sync + 'static,
    U: Clone + Send + Sync + 'static,
    V: Send + 'static,
{
    // The task in charge of connecting
    async fn connect(&self) {
        // Retrieves the connection parameters.
        let connection_parameters = {
            let mut requester = self.0.write().await;
            match *requester {
                // If connection is working stop trying to connect(Multiple task can exists trying to reconnect)
                ConnectionWrapper::Connected(_) => {
                    return;
                }
                ConnectionWrapper::Disconnected(ref mut disconnected) => {
                    // If a reconnection is already in progress let the task doing that continue and stop this one
                    if disconnected.reconnection_in_progress {
                        tracing::debug!("Reconnection in progress");
                        return;
                    }

                    disconnected.reconnection_in_progress = true;
                    disconnected.connection_parameters.clone()
                }
            }
        };

        // Try to connect with exponential backoff without limits(Using default parameters)
        let connection = backoff::future::retry(
            ExponentialBackoff {
                max_elapsed_time: None,
                ..Default::default()
            },
            || async { T::try_connect(connection_parameters.clone()).await },
        )
        .await;

        match connection {
            // If connected store the new connection.
            Ok(connection) => {
                *self.0.write().await = ConnectionWrapper::Connected(Connected {
                    connection,
                    connection_parameters,
                });
            }

            // If couldn't connect due to a permanent error set the connection as not in progress and return
            Err(_) => {
                if let ConnectionWrapper::Disconnected(Disconnected {
                    ref mut reconnection_in_progress,
                    ..
                }) = *self.0.write().await
                {
                    *reconnection_in_progress = false;
                }
            }
        }
    }

    /// Creates a new connector and start connection process in the background.
    pub fn new(connection_parameters: U) -> Self {
        let request = ConnectionWrapper::Disconnected(Disconnected {
            reconnection_in_progress: false,
            connection_parameters,
        });

        let request = Connector(Arc::new(RwLock::new(request)));
        let request_closure = request.clone();
        tokio::spawn(async move { request_closure.connect().await });
        request
    }

    // This method is used by the trait implementations in the Connector to pass a lambda that is used by the connection.
    // It uses the connection with the given method and if the connection fails it returns the error so a reconnection can start otherwise
    // it submits the result.
    pub(crate) async fn execute<F, R: 'static, E: Into<Error<E>> + 'static>(
        &self,
        f: F,
    ) -> Result<R, RequestError<E>>
    where
        F: Send + FnOnce(&T) -> BoxFuture<'_, Result<R, E>>,
    {
        let err;
        {
            // Get the connection
            let connection = self.0.read().await;
            // If the connection works try to use it otherwise return the no connection error.
            err = if let ConnectionWrapper::Connected(ref conn) = *connection {
                let res;
                {
                    let conn = &conn.connection;
                    res = f(conn).await;
                }
                // Return from the function with the result or store the error in `err`.
                match res {
                    Ok(r) => return Ok(r),
                    Err(e) => match e.into() {
                        // A permanent error means that there is a problem with the submission itself
                        Error::Permanent(e) => return Err(RequestError::SubmitionProblem(e)),
                        // A transient error will probably mean a disconnection(TODO: Be more precise when we need a reconnection)
                        Error::Transient { err, .. } => err,
                    },
                }
            } else {
                return Err(RequestError::NoConnection);
            }
        }

        // If the function didn't return here it means that the connection is disconnected(do that)
        {
            let mut requester = self.0.write().await;
            requester.disconnect();
        }

        // Initiate re-connection
        let requester = self.clone();
        tokio::spawn(async move { requester.connect().await });

        Err(RequestError::GotDisconnected(err))
    }
}

impl<T, U, V> ConnectionWrapper<T, U, V>
where
    T: TryConnect<ConnectionParameters = U, ConnectionError = V> + Send + Sync + 'static,
    U: Clone + Send + Sync + 'static,
    V: Send + 'static,
{
    fn disconnect(&mut self) {
        if let Self::Connected(conn) = self {
            *self = Self::Disconnected(Disconnected {
                reconnection_in_progress: false,
                connection_parameters: conn.connection_parameters.clone(), // TODO: This clone could be prevented
            });
        }
    }
}

// == Here are trait implementations for the connector through which it is used ==
// Always prefer an implementation on the `Connector` rather than an implementation on a client itself since the connector has the additional reconnection logic.
#[async_trait]
impl<T> ChainConnection for Connector<Client<T>, String, subxt::BasicError>
where
    T: subxt::Config + Send + Sync,
    <T as subxt::Config>::Address: Send,
    <T as subxt::Config>::Hash: Send,
{
    type Transaction = UncheckedExtrinsic<T>;

    type Hash = <T as subxt::Config>::Hash;

    // We don't test connections
    #[cfg(not(tarpaulin_include))]
    async fn submit_transaction(&self, tx: Self::Transaction) -> ConnectionResult<Self::Hash> {
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
impl<T> ContextDataProvider for Connector<Client<T>, String, subxt::BasicError>
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
pub trait TryConnect: Sized {
    type ConnectionParameters;
    type ConnectionError;
    async fn try_connect(
        params: Self::ConnectionParameters,
    ) -> Result<Self, Error<Self::ConnectionError>>;
}

#[async_trait]
impl<T: subxt::Config + Send + Sync> TryConnect for Client<T> {
    type ConnectionParameters = String;
    type ConnectionError = subxt::BasicError;

    // We don't test connections
    #[cfg(not(tarpaulin_include))]
    async fn try_connect(
        params: Self::ConnectionParameters,
    ) -> Result<Self, Error<Self::ConnectionError>> {
        match ClientBuilder::default().set_url(&params).build().await {
            Ok(client) => Ok(client),
            Err(e) => {
                tracing::error!("Error while trying to connect client to blockchain node: {e:?}");
                Err(Error::transient(e))
            }
        }
    }
}

#[cfg(test)]
impl Connector<MockChainConnection, String, subxt::BasicError> {
    pub(crate) async fn get_transactions(&self) -> Option<Vec<UncheckedExtrinsic<DefaultConfig>>> {
        match *self.0.read().await {
            ConnectionWrapper::Connected(ref conn) => Some(conn.connection.get_transactions()),
            _ => None,
        }
    }
}
