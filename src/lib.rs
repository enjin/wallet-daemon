#![allow(clippy::too_many_arguments)]
#![allow(long_running_const_eval)]

use subxt::{OfflineClient, SubstrateConfig};
use crate::config_loader::{Chain, ConfigNetwork};
pub use importer::write_seed;
pub use multitenant::set_multitenant;
pub use platform_client::{populate_managed_wallets, sign_transactions};
pub use transaction::TransactionJob;
pub use wallet::DeriveWalletJob;
use crate::substrate_client::EnjinConfig;

pub type SubstrateClient = OfflineClient<EnjinConfig>;

pub const CHAIN: Chain = Chain::Matrix; // Matrix or Relay
pub const NETWORK: ConfigNetwork = ConfigNetwork::Enjin; // Canary or Enjin
/// The number of blocks that the signed tx is mortal for
pub const TX_MORTALITY: u64 = 64;
/// The number blocks that the dummy signed tx is mortal for
pub const DUMMY_TX_MORTALITY: u64 = 14_400;

pub mod config_loader;
mod graphql;
mod importer;
mod multitenant;
mod subscription;
pub mod substrate_client;
mod platform_client;
mod transaction;
mod wallet;
