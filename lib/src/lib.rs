//! This crate provides handlers and types for the Efinity Wallet
//!
//! # Examples
//!
//! ```no_run
//! # use tokio::signal;
//! # use wallet_lib::*;
//! # use std::{sync::Arc, time::Duration};
//! # use subxt::DefaultConfig;
//! # use wallet_lib::wallet_trait::Wallet;
//! # let rt = tokio::runtime::Runtime::new().unwrap();
//! # rt.block_on(async {
//!    // Load wallet, returning the graphql_endpoint(for the open platform)
//!    // and the connection_pair(The wallet + the connection to the blockchain)
//!    let (connection_pair, graphql_endpoint, token) =
//!        load_wallet::<DefaultConfig>(load_config()).await;
//!
//!
//!    // Create Job pair: Signing & Polling
//!    let (poll_job, sign_processor) = create_job_pair(
//!        graphql_endpoint,
//!        token,
//!        String::from("matrix"),
//!        Duration::from_millis(6000),
//!        Arc::new(connection_pair),
//!        10,
//!    );
//!
//!    // Start jobs
//!    poll_job.start_job();
//!    sign_processor.start_job();
//!
//!    // Run until Ctrl+C is pressed
//!    signal::ctrl_c().await.expect("Failed to listen for ctrl c");
//! # });
//! ```

pub mod config_loader;
mod connection;
#[cfg(not(tarpaulin_include))]
mod extra;
mod jobs;
mod types;
pub(crate) mod wallet;

#[cfg(not(tarpaulin_include))]
pub mod mock;
#[cfg(test)]
mod tests;

pub(crate) use connection::{chain_connection, client_connection, connection_handler};
pub(crate) use types::{SignedExtra, UncheckedExtrinsic};

pub use crate::wallet_trait::EfinityWallet;
pub use config_loader::{load_config, load_relay_wallet, load_wallet};
pub use jobs::{
    create_job_pair, create_wallet_job_pair, DeriveWalletProcessor, PollJob, PollWalletJob,
    SignProcessor,
};
pub use types::PairSigner;
pub use wallet::wallet_trait;
