#![allow(clippy::too_many_arguments)]
use crate::config_loader::{Chain, ConfigNetwork};
pub use importer::write_seed;
pub use multitenant::set_multitenant;
pub use platform_client::{populate_managed_wallets, sign_transactions};
pub use subscription::{SubscriptionJob, SubscriptionParams};
pub use transaction::TransactionJob;
pub use wallet::DeriveWalletJob;

pub const CHAIN: Chain = Chain::Matrix; // Matrix or Relay
pub const NETWORK: ConfigNetwork = ConfigNetwork::Enjin; // Canary or Enjin

pub mod config_loader;
mod graphql;
mod importer;
mod multitenant;
mod subscription;

mod platform_client;
mod transaction;
mod wallet;
