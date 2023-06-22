//! Traits related to the wallet-side
use crate::PairSigner;
use crate::SignedExtra;
use crate::UncheckedExtrinsic;
use async_trait::async_trait;
use futures::TryFutureExt;
use lru::LruCache;
use sp_application_crypto::DeriveJunction;
use std::fmt::Display;
use std::iter;
use std::sync::Arc;
use std::{fmt::Debug, sync::Mutex};
use subxt::extrinsic::create_signed;
use subxt::rpc::RuntimeVersion;
use subxt::sp_core::Pair;
use subxt::sp_runtime::traits::{Header, IdentifyAccount, One, Verify, Zero};
use subxt::{Client, Signer};
use thiserror::Error;

/// Result of a wallet operation
pub type WalletResult<T> = Result<T, WalletError>;

/// Represent something that can sign transactions.
#[async_trait]
pub trait Wallet {
    /// Type of the unsigned transaction
    /// Normaly `Vec<u8>`
    type UnsignedEncodedTransaction;

    /// Type for signed transaction
    type SignedTransaction;

    /// Account Id Type
    type AccountId;

    /// Signs a given transaction with the given period to live.
    async fn sign_transaction(
        &self,
        encoded_tx: Self::UnsignedEncodedTransaction,
    ) -> WalletResult<Self::SignedTransaction>;
    async fn get_block_number(&self) -> WalletResult<u64>;
    /// Returns the public key of the wallet
    async fn account_id(&self) -> WalletResult<&Self::AccountId>;
    /// Reset's wallet nonce to the one stored in the node
    async fn reset_nonce(&self) -> WalletResult<()>;
}

/// This traits represent something that can derive a new key from an existing key.
pub trait KeyDerive<T>
where
    T: subxt::Config,
{
    /// Error related to derivation.
    type DeriveError;
    /// Type for the seed that will be used to create the new key.
    type Seed;
    /// Type of the new "Signer" that will be created.
    type NewSigner;
    /// Takes a "path" and a seed and derives a new key.
    #[allow(clippy::type_complexity)]
    fn derive(
        &self,
        path: DeriveJunction,
        seed: Option<Self::Seed>,
    ) -> Result<(Self::NewSigner, Option<Self::Seed>), Self::DeriveError>;
}

impl<T, U> KeyDerive<T> for PairSigner<T, U>
where
    T: subxt::Config,
    U: subxt::sp_core::Pair,
    T::Signature: From<U::Signature>,
    <T::Signature as Verify>::Signer: From<U::Public> + IdentifyAccount<AccountId = T::AccountId>,
    T::Signature: Verify,
{
    type DeriveError = <U as Pair>::DeriveError;
    type Seed = <U as Pair>::Seed;
    type NewSigner = PairSigner<T, U>;

    #[allow(clippy::type_complexity)]
    fn derive(
        &self,
        path: DeriveJunction,
        seed: Option<Self::Seed>,
    ) -> Result<(Self::NewSigner, Option<Self::Seed>), Self::DeriveError> {
        let (pair, seed) = Pair::derive(self.signer(), iter::once(path), seed)?;
        let pair = PairSigner::new(pair);
        Ok((pair, seed))
    }
}

/// Implementation of [`Wallet`](Wallet) that works particularly for efinity
///
/// This can work with multiple signers(Right now only [PairSigner])
///
/// Note: The way the wallet handles the nonce is
/// Every time it's going to sign a transaction it gets the nonce from the [ContextDataProvider] (This will get it from the nonce).
/// Then it locks the stored nonce(Could use an atomic but this is easier) and compares it with the provided from the nonce and keeps the biggest one, increases it by 1 and use that one to sign the transaction.
///
/// ### Nonce example
/// Take the current state of nonces to be:
///
/// ```ignore
/// Local -> 10
/// Node -> 7
/// ```
///
/// * [EfinityWallet::sign_transaction] is called.
/// * Node nonce is retrieved(7)
/// * Local nonce is locked (10)
/// * Compare local to node's nonce, 10 > 7, then use 10 to sign and new state is:
///
/// ```ignore
/// Local -> 11
/// Node -> 7
/// ```
///
/// Additional Note: In `jobs` when any transaction submision fails the nonce is resetted, that might make every new transaction fail after that until a new block is produced and the nonce is updated in the node.
/// This seems to be the safer way to keep track of the nonce.
pub struct EfinityWallet<T, U, V>
where
    T: subxt::Config,
    V: ContextDataProvider<
        ContextData = EfinityContextData<T::Hash, T::Index>,
        AccountId = T::AccountId,
        Nonce = T::Index,
    >,
{
    context_provider: Arc<V>,
    signer: U,
    latest_nonce: Arc<Mutex<T::Index>>, // Could be optimized with an AtomicU32 but let's not deal with that before benchmarking
    // Note the LruCache is used instead of a Vec to prevent unbounded memory growth.
    children_nonces: Mutex<LruCache<DeriveJunction, Arc<Mutex<T::Index>>>>,
}

impl<T, U, V> Debug for EfinityWallet<T, U, V>
where
    T: subxt::Config,
    V: ContextDataProvider<
        ContextData = EfinityContextData<T::Hash, T::Index>,
        AccountId = T::AccountId,
        Nonce = T::Index,
    >,
    U: Signer<T, SignedExtra<T>>,
    V: Debug,
{
    // We don't test debug prints
    #[cfg(not(tarpaulin_include))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EfinityWallet")
            .field("context_provider", &self.context_provider)
            .field("account_id", &self.signer.account_id())
            .field("latest_nonce", &self.latest_nonce)
            .field("children_nonces", &self.children_nonces)
            .finish()
    }
}

/// A [Wallet] that supports [KeyDerive]
pub trait WalletWithKeyDerive<T: subxt::Config>: Wallet + KeyDerive<T> {}

impl<T, U, V> KeyDerive<T> for EfinityWallet<T, U, V>
where
    T: subxt::Config + Sync + Send,
    T::AccountId: Display,
    U: KeyDerive<T>
        + KeyDerive<T, NewSigner = U>
        + Signer<T, SignedExtra<T>>
        + std::marker::Send
        + std::marker::Sync
        + 'static,
    T::Extrinsic: Send + Sync,
    T::BlockNumber: Into<u64>,
    V: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        > + Send
        + Sync
        + 'static,
{
    type DeriveError = <U as KeyDerive<T>>::DeriveError;

    type Seed = <U as KeyDerive<T>>::Seed;

    type NewSigner = Self;

    #[allow(clippy::type_complexity)]
    fn derive(
        &self,
        path: DeriveJunction,
        seed: Option<Self::Seed>,
    ) -> Result<(Self::NewSigner, Option<Self::Seed>), Self::DeriveError> {
        let mut children_nonces = self.children_nonces.lock().unwrap();
        // Note: LruCache doesn't support the `Entry` interface.
        if matches!(children_nonces.get(&path), None) {
            children_nonces.put(path, Default::default());
        }

        // We just added the path in the lines above
        let nonce = children_nonces.get(&path).unwrap().clone();
        let (signer, seed) = self.signer.derive(path, seed)?;

        let wallet = Self::new_with_nonce(&self.context_provider, signer, Some(nonce));
        Ok((wallet, seed))
    }
}

impl<T, U: Wallet + KeyDerive<T>> WalletWithKeyDerive<T> for U where T: subxt::Config {}

const CONCURRENT_PLAYERS: usize = 100_000;

impl<T, U, V> EfinityWallet<T, U, V>
where
    T: subxt::Config,
    V: ContextDataProvider<
        ContextData = EfinityContextData<T::Hash, T::Index>,
        AccountId = T::AccountId,
        Nonce = T::Index,
    >,
{
    /// Creates a new [Efinity Wallet](EfinityWallet)
    /// Provided a [ContextDataProvider] (This will probably be a [Client])
    /// And a [Signer](Signer), namely, [crate::config_loader::PairSig]
    ///
    /// ## Example
    /// ```
    /// # use std::sync::Arc;
    /// # use wallet_lib::mock::MockEfinityDataProvider;
    /// # use wallet_lib::PairSigner;
    /// # use wallet_lib::EfinityWallet;
    /// # use sp_keyring::AccountKeyring;
    /// # use subxt::DefaultConfig;
    /// let context_provider = Arc::new(MockEfinityDataProvider::<subxt::DefaultConfig>::new());
    /// let signer = PairSigner::<DefaultConfig, _>::new(AccountKeyring::Alice.pair());
    /// EfinityWallet::<DefaultConfig, _, _>::new(&context_provider, signer);
    /// ```
    pub fn new(connection: &Arc<V>, signer: U) -> Self {
        Self::new_with_nonce(connection, signer, None)
    }

    /// Same as [Self::new] but you can provide a default nonce but you can safely disregard this.
    ///
    /// It's mainly used when deriving children wallets (Not currently supported by the open-platform).
    ///
    /// ## Example
    /// ```
    /// # use std::sync::Arc;
    /// # use wallet_lib::mock::MockEfinityDataProvider;
    /// # use wallet_lib::PairSigner;
    /// # use wallet_lib::EfinityWallet;
    /// # use sp_keyring::AccountKeyring;
    /// # use subxt::DefaultConfig;
    /// let context_provider = Arc::new(MockEfinityDataProvider::<subxt::DefaultConfig>::new());
    /// let signer = PairSigner::<DefaultConfig, _>::new(AccountKeyring::Alice.pair());
    /// EfinityWallet::<DefaultConfig, _, _>::new_with_nonce(&context_provider, signer, None);
    /// ```
    pub fn new_with_nonce(
        connection: &Arc<V>,
        signer: U,
        nonce: Option<Arc<Mutex<T::Index>>>,
    ) -> Self {
        Self {
            context_provider: Arc::clone(connection),
            signer,
            latest_nonce: nonce.unwrap_or_default(),
            children_nonces: Mutex::new(LruCache::new(CONCURRENT_PLAYERS)),
        }
    }
}

#[async_trait]
impl<T, U, V> Wallet for EfinityWallet<T, U, V>
where
    T: subxt::Config + Sync + Send,
    <T as subxt::Config>::AccountId: Display + Sync + Clone,
    U: Signer<T, SignedExtra<T>> + Send + Sync,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
    V: ContextDataProvider<
            ContextData = EfinityContextData<T::Hash, T::Index>,
            AccountId = T::AccountId,
            Nonce = T::Index,
        > + Sync
        + Send,
{
    // TODO: Could we prevent allocation?
    // TBH This is probably NOT the bottleneck for now
    type UnsignedEncodedTransaction = Vec<u8>;

    type SignedTransaction = UncheckedExtrinsic<T>;

    type AccountId = <T as subxt::Config>::AccountId;

    async fn sign_transaction(
        &self,
        encoded_tx: Self::UnsignedEncodedTransaction,
    ) -> WalletResult<Self::SignedTransaction> {
        let metadata = ContextDataProvider::get_context_data(
            &*self.context_provider,
            self.account_id().await?,
        )
        .await?;

        // Correct nonce based on local count
        let nonce;
        {
            let mut latest_nonce = self.latest_nonce.lock().unwrap();
            nonce = latest_nonce.max(metadata.nonce);
            *latest_nonce = nonce + One::one();
        }
        let account_id = self.account_id().await;
        tracing::debug!("Using nonce: {nonce:?}, with {account_id:?}"); // TODO
        Ok(create_signed(
            &metadata.runtime_version,
            metadata.genesis_hash,
            nonce,
            subxt::Encoded(encoded_tx),
            &self.signer,
            (metadata.block_number, 16, metadata.block_hash),
        )
        .await?)
    }

    async fn get_block_number(&self) -> WalletResult<u64> {
        let metadata = ContextDataProvider::get_context_data(
            &*self.context_provider,
            self.account_id().await?,
        )
        .await?;

        Ok(metadata.block_number)
    }

    async fn account_id(&self) -> WalletResult<&Self::AccountId> {
        // TODO: This might need to be retrieved in the moment
        Ok(self.signer.account_id())
    }

    async fn reset_nonce(&self) -> WalletResult<()> {
        let mut latest_nonce = self.latest_nonce.lock().unwrap();
        *latest_nonce = Zero::zero();
        Ok(())
    }
}

/// Errors related to the wallet
#[derive(Error, Debug)]
pub enum WalletError {
    /// Errors related to the [subxt::Client]
    #[error("Problem while fetching metadata")]
    ClientError(#[from] subxt::BasicError),
    /// Error due to client disconnection
    #[error("Disconnected wallet")]
    Disconnected,
}

/// Relevant context data provided by an Efinity nonce to sign a transaction
#[derive(Debug, Clone)]
pub struct EfinityContextData<Hash, Nonce> {
    /// Current nonce in the blockchain
    pub nonce: Nonce,
    /// Runtime version
    pub runtime_version: RuntimeVersion,
    /// Genesis Hash
    pub genesis_hash: Hash,
    /// Current block number
    pub block_number: u64,
    /// Current block hash
    pub block_hash: Hash,
}

/// A trait that returns the context necessary to sign an extrinsic
#[async_trait]
pub trait ContextDataProvider {
    /// The context necessary to sign an extrinsic
    type ContextData;

    /// Account Id of the to-sign account
    type AccountId;

    /// Nonce type
    type Nonce;

    /// Returns the Context Data
    /// Can fail if there's any problem with the connection
    /// Or if the account doesn't exists. (This means that the account needs to exists before
    /// signing transactions which is a reasonable assumption since it needs funds to be able to
    /// meaningfully sign)
    async fn get_context_data(&self, account: &Self::AccountId) -> WalletResult<Self::ContextData>;
}

/// Represent a type that can return a nonce for a given account
#[async_trait]
pub trait NonceProvider {
    /// Account Id type for getting the nonce
    type AccountId;
    /// Nonce type
    type Nonce;

    /// Retrieves the nonce for the account
    /// Can fail if the provider is disconnected
    async fn get_nonce(&self, account: &Self::AccountId) -> WalletResult<Self::Nonce>;
}

#[async_trait]
impl<T> NonceProvider for Client<T>
where
    T: subxt::Config + Send + Sync,
    <T as subxt::Config>::AccountId: Sync + Clone,
{
    type AccountId = <T as subxt::Config>::AccountId;
    type Nonce = <T as subxt::Config>::Index;

    // We don't want to do test that require connections
    #[cfg(not(tarpaulin_include))]
    async fn get_nonce(&self, account: &Self::AccountId) -> WalletResult<T::Index> {
        Ok(self.rpc().system_account_next_index(account).await?)
    }
}

#[async_trait]
impl<T> ContextDataProvider for Client<T>
where
    T: subxt::Config + Send + Sync,
    <T as subxt::Config>::AccountId: Sync + Clone,
    Client<T>: NonceProvider<AccountId = T::AccountId, Nonce = T::Index>,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
{
    type ContextData = EfinityContextData<<T as subxt::Config>::Hash, <T as subxt::Config>::Index>;
    type AccountId = <T as subxt::Config>::AccountId;
    type Nonce = <T as subxt::Config>::Index;

    // We don't want to do test that require connections
    #[cfg(not(tarpaulin_include))]
    async fn get_context_data(&self, account: &Self::AccountId) -> WalletResult<Self::ContextData> {
        let block = self
            .rpc()
            .block(None)
            .await?
            .ok_or_else(|| subxt::BasicError::Other("No block".to_string()))?
            .block;

        let block_hash = block.header.hash();

        let runtime_version = self
            .rpc()
            .runtime_version(Some(block_hash))
            .map_err(WalletError::from);
        let genesis_hash = self.rpc().genesis_hash().map_err(WalletError::from);
        let nonce = NonceProvider::get_nonce(self, account);

        let (runtime_version, genesis_hash, nonce) =
            tokio::try_join!(runtime_version, genesis_hash, nonce)?;

        let block_number = (*block.header.number()).into();
        Ok(EfinityContextData {
            nonce,
            runtime_version,
            genesis_hash,
            block_number,
            block_hash,
        })
    }
}
