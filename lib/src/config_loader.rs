//! Utility functions to load and parse configuration
use config::Config;
use sc_keystore::LocalKeystore;
use secrecy::SecretString;
use serde::Deserialize;
use sp_application_crypto::AppKey;
use sp_application_crypto::{
    sr25519::AppPair as Sr25519AppPair, sr25519::Pair as Sr25519Pair, Pair, Ss58Codec,
};
use sp_core::crypto::Ss58AddressFormat;
use sp_keystore::CryptoStore;
use std::env;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use subxt::sp_runtime::traits::{IdentifyAccount, Verify};
use subxt::{Client, Signer};

use crate::wallet_trait::{ContextDataProvider, EfinityContextData};
use crate::{
    chain_connection::ChainConnection as ChainConn, connection_handler::Connector,
    wallet_trait::EfinityWallet,
};
use crate::{PairSigner, UncheckedExtrinsic};

/// Load the config either from the default location or from the one specified in CONFIG_FILE
/// # Examples
/// ``` no_run
/// # use wallet_lib::load_config;
/// let config = load_config();
/// ```
pub fn load_config() -> Configuration {
    let config_file = env::var(CONFIG_FILE_ENV_NAME).unwrap_or_else(|_| {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(DEFAULT_CONFIG_FILE);
        p.to_str().unwrap().to_string()
    });

    let config = Config::builder()
        .add_source(config::File::from(Path::new(&config_file)))
        .build()
        .expect("Configuration file not found");
    config
        .try_deserialize::<Configuration>()
        .expect("Configuration is incorrect")
}

/// Env name to override default path for config file.
pub(crate) const CONFIG_FILE_ENV_NAME: &str = "CONFIG_FILE";

/// Default name of config file.
pub(crate) const DEFAULT_CONFIG_FILE: &str = "config.json";

/// Env name for master key
pub(crate) const KEY_PASS: &str = "KEY_PASS";

/// Env name for open platform static token
pub(crate) const PLATFORM_KEY: &str = "PLATFORM_KEY";

pub(crate) type ChainConnector<T> = Connector<Client<T>, String, subxt::BasicError>;

/// Represents a pair of connection/wallet.
///
/// A given wallet will should correspond to the connection.
///
/// Any wallet that is associated with the wallet will be related to the same connection.
pub struct WalletConnectionPair<T, ChainConnection, ContextProvider>
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
    >,
{
    /// Efinity related wallet
    pub wallet: EfinityWallet<T, PairSig<T>, ContextProvider>,
    /// Connection to Efinity
    pub connection: Arc<ChainConnection>,
}

impl<T, ChainConnection, ContextProvider> Debug
    for WalletConnectionPair<T, ChainConnection, ContextProvider>
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
        > + Debug,
    ChainConnection: Debug,
{
    // We don't test debug prints
    #[cfg(not(tarpaulin_include))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletConnectionPair")
            .field("wallet", &self.wallet)
            .field("connection", &self.connection)
            .finish()
    }
}

/// Local signer generally used(Sr25519)
pub type PairSig<T> = PairSigner<T, Sr25519Pair>;

/// Configuration for the daemon.
///
/// See the `config.json` file on the root directory to see an example.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Configuration {
    /// Url of the matrix.
    node: String,
    /// Url of the relay.
    relay_node: String,
    /// Stored key path
    master_key: PathBuf,
    /// Platform GraphQL endpoint.
    api: String,
}

#[cfg(test)]
impl Configuration {
    pub(crate) fn new(node: String, relay_node: String, master_key: PathBuf, api: String) -> Self {
        Self {
            node,
            relay_node,
            master_key,
            api,
        }
    }
}

// Loads the signer for a key from keystore
async fn get_keys<T>(key_store_path: &Path, password: SecretString) -> PairSig<T>
where
    T: subxt::Config,
    T::Signature: From<<Sr25519Pair as Pair>::Signature> + Verify,
    <T::Signature as Verify>::Signer:
        From<<Sr25519Pair as Pair>::Public> + IdentifyAccount<AccountId = T::AccountId>,
    <T::Signature as subxt::sp_runtime::traits::Verify>::Signer: From<sp_core::sr25519::Public>,
{
    let key_store = LocalKeystore::open(key_store_path, Some(password))
        .expect("Couldn't read or open keystore");

    // For now only supporting sr25519 works
    // Also 1 key per store
    let public_key = if let Some(public_key) = key_store
        .sr25519_public_keys(Sr25519AppPair::ID)
        .await
        .first()
    {
        *public_key
    } else {
        key_store
            .sr25519_generate_new(Sr25519AppPair::ID, None)
            .await
            .expect("Couldn't generate key")
    };

    // Note on types: We need T: AppPair for `LocalKeyStore::key_pair(T)` and that's only implemented for by the *AppPair(where * is the key type)
    // but `PairSigner::new(T)` requires `T::Signature: From<P::Signature>` and `T::Signature` in most cases is `MultiSignature` and that is only implemented for `P = *Pair`
    // (Not *AppPair) where * is the key type.
    PairSig::new(
        key_store
            .key_pair::<Sr25519AppPair>(&public_key.into())
            .expect("Error generating key")
            .expect("Key doesnt exists[This should never happen]")
            .into(),
    )
}

// Consumes the password from the enviroment variable and returns it as a secretstring
fn get_password(env_name: &str) -> SecretString {
    let password = SecretString::new(
        env::var(env_name).unwrap_or_else(|_| panic!("Password {} not loaded in memory", env_name)),
    );
    env::remove_var(env_name);
    password
}

/// Loads the wallet and connection using the passed `configuration`.
///
/// It also returns a `graphql_endpoint` which is just the endpoint for the open-platform set in the config.
///
/// # Example
///
/// ```no_run
/// # use wallet_lib::{load_config, load_wallet};
/// # let rt = tokio::runtime::Runtime::new().unwrap();
/// # rt.block_on(async {
/// let (connection_pair, graphql_endpoint, token) = load_wallet::<subxt::DefaultConfig>(load_config()).await;
/// # });
/// ```
pub async fn load_wallet<T>(
    config: Configuration,
) -> (
    WalletConnectionPair<T, ChainConnector<T>, ChainConnector<T>>,
    String,
    String,
)
where
    T: subxt::Config + Sync + Send,
    T::Signature: From<<Sr25519Pair as Pair>::Signature> + Verify + From<sp_core::ecdsa::Signature>,
    <T::Signature as Verify>::Signer:
        From<<Sr25519Pair as Pair>::Public> + IdentifyAccount<AccountId = T::AccountId>,
    T::AccountId: Into<<T as subxt::Config>::Address>
        + std::fmt::Display
        + From<subxt::sp_runtime::AccountId32>,
    T::Address: From<T::AccountId> + Send + Sync,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
{
    let context_provider = Arc::new(Connector::<Client<T>, String, subxt::BasicError>::new(
        config.node.to_owned(),
    ));
    let connection = Arc::clone(&context_provider);

    load_wallet_with_connections(config, context_provider, connection).await
}

pub(crate) async fn load_wallet_with_connections<T, ChainConnection, ContextProvider>(
    config: Configuration,
    context_provider: Arc<ContextProvider>,
    chain_connection: Arc<ChainConnection>,
) -> (
    WalletConnectionPair<T, ChainConnection, ContextProvider>,
    String,
    String,
)
where
    T: subxt::Config + Sync + Send,
    T::Signature: From<<Sr25519Pair as Pair>::Signature> + Verify + From<sp_core::ecdsa::Signature>,
    <T::Signature as Verify>::Signer:
        From<<Sr25519Pair as Pair>::Public> + IdentifyAccount<AccountId = T::AccountId>,
    T::AccountId: Into<<T as subxt::Config>::Address>
        + std::fmt::Display
        + From<subxt::sp_runtime::AccountId32>,
    T::Address: From<T::AccountId> + Send + Sync,
    T::Extrinsic: Send,
    T::BlockNumber: Into<u64>,
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
    let key_path = config.master_key;

    let token = env::var(PLATFORM_KEY).unwrap_or_default();
    let password = get_password(KEY_PASS);
    let signer = get_keys::<T>(&key_path, password).await;

    let account_id = signer.account_id();
    let converted_id: sp_core::crypto::AccountId32 =
        Ss58Codec::from_ss58check(&*account_id.to_string()).unwrap();

    println!("Wallet daemon address in different formats:");
    println!(
        "Matrix: {}",
        converted_id.to_ss58check_with_version(Ss58AddressFormat::custom(1110))
    );
    println!(
        "Canary: {}",
        converted_id.to_ss58check_with_version(Ss58AddressFormat::custom(9030))
    );

    let wallet = EfinityWallet::new(&context_provider, signer);
    let wallet_connection_pair = WalletConnectionPair {
        wallet,
        connection: Arc::clone(&chain_connection),
    };

    (wallet_connection_pair, config.api, token)
}
