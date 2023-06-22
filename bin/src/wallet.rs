//! Wallet daemon for efinity blockchain
//!
//! It polls for transactions, signs them and sends them to the blockchain.
//!
//! A configuration file is needed(See README for specifics)

use std::{sync::Arc, time::Duration};

use subxt::DefaultConfig;
use tokio::signal;
use wallet_lib::*;

/// Time in milliseconds that a block is expected to be generated.
const BLOCKTIME_MS: u64 = 12000;

/// Page size for the number of transaction requests that are brought with each polling.
const PAGE_SIZE: usize = 40;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let (wallet_pair, graphql_endpoint, token) = load_wallet::<DefaultConfig>(load_config()).await;
    let wallet_pair = Arc::new(wallet_pair);

    let (poll_job, sign_processor) = create_job_pair(
        graphql_endpoint.clone(),
        String::from(token.clone()),
        Duration::from_millis(BLOCKTIME_MS),
        Arc::clone(&wallet_pair),
        PAGE_SIZE.try_into().unwrap(),
    );

    let (poll_wallet_job, derive_wallet_processor) = create_wallet_job_pair(
        graphql_endpoint,
        String::from(token.clone()),
        Duration::from_millis(BLOCKTIME_MS),
        Arc::clone(&wallet_pair),
        PAGE_SIZE.try_into().unwrap(),
    );

    poll_job.start_job();
    sign_processor.start_job();

    poll_wallet_job.start_job();
    derive_wallet_processor.start_job();

    signal::ctrl_c().await.expect("Failed to listen for ctrl c");
}
