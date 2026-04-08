#![allow(missing_docs, long_running_const_eval, clippy::too_many_arguments)]

use std::env;
use std::process::exit;
use std::sync::Arc;
use subxt::OfflineClient;

use crate::config_loader::{load_config, load_wallet};
use crate::importer::write_seed;
use crate::multitenant::set_multitenant;
use crate::substrate_client::EnjinConfig;
use crate::transaction::TransactionJob;
use crate::wallet::DeriveWalletJob;

pub type SubstrateClient = OfflineClient<EnjinConfig>;

pub const CHAIN: config_loader::Chain = config_loader::Chain::Matrix;
pub const NETWORK: config_loader::ConfigNetwork = config_loader::ConfigNetwork::Enjin;
pub const TX_MORTALITY: u64 = 64;
pub const DUMMY_TX_MORTALITY: u64 = 14_400;

mod config_loader;
mod graphql;
mod importer;
mod multitenant;
mod subscription;
pub mod substrate_client;
mod platform_client;
mod transaction;
mod wallet;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Some(arg) = args.first() {
        if arg == "import" {
            println!("Enjin Platform - Import Wallet");
            let seed = rpassword::prompt_password("Please type your 12-word mnemonic: ").unwrap();
            write_seed(seed).expect("Failed to import your wallet");

            exit(1);
        }
    }

    let (keypair, matrix_url, platform_url, platform_token) = load_wallet(load_config()).await;

    tracing_subscriber::fmt::init();
    set_multitenant(
        keypair.clone(),
        platform_url.clone(),
        platform_token.clone(),
    )
    .await;
    let matrix_client = substrate_client::setup_client(&matrix_url).await;

    let (matrix_tx_poller, matrix_tx_processor) = TransactionJob::create_job(
        Arc::clone(&matrix_client),
        keypair.clone(),
        platform_url.clone(),
        platform_token.clone(),
    );

    let (wallet_poller, wallet_processor) =
        DeriveWalletJob::create_job(keypair, platform_url, platform_token);

    tokio::select! {
        _ = matrix_tx_poller.start() => {}
        _ =  matrix_tx_processor.start() => {}
        _ = wallet_poller.start() => {}
        _ = wallet_processor.start() => {}
    }

    Ok(())
}