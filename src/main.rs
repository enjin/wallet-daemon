#![allow(missing_docs, long_running_const_eval, clippy::too_many_arguments)]

use std::env;
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use std::sync::Arc;
use subxt::OfflineClient;

use crate::importer::write_seed;
use crate::multitenant::set_multitenant;
use crate::substrate_client::EnjinConfig;
use crate::transaction::TransactionJob;
use crate::wallet::DeriveWalletJob;
use crate::wallet_loader::load_wallet;

pub type SubstrateClient = OfflineClient<EnjinConfig>;

pub const CHAIN: wallet_loader::Chain = wallet_loader::Chain::Matrix;
pub const NETWORK: wallet_loader::ConfigNetwork = wallet_loader::ConfigNetwork::Enjin;
pub const DEFAULT_PLATFORM_URL: &str = "https://platform.enjin.io/graphql/daemon";
pub const TX_MORTALITY: u64 = 64;
pub const DUMMY_TX_MORTALITY: u64 = 14_400;

mod global;
mod graphql;
mod importer;
mod multitenant;
mod platform_client;
mod subscription;
pub mod substrate_client;
mod transaction;
mod wallet;
mod wallet_loader;

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

    let keypair = {
        let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
        let master_key = dotenvy::var("MASTER_KEY").unwrap_or("store".to_string());
        let master_key = PathBuf::from_str(&master_key).expect("MASTER_KEY must be a valid path");
        load_wallet(&master_key, &key_pass).await
    };
    let platform_key = dotenvy::var("PLATFORM_KEY").expect("PLATFORM_KEY env var is required");
    let platform_token = format!("Bearer {}", platform_key);
    let platform_url = dotenvy::var("PLATFORM_URL").unwrap_or(DEFAULT_PLATFORM_URL.to_string());
    println!("** Platform URL: {}", platform_url);
    println!("*****************************************************************");

    tracing_subscriber::fmt::init();
    set_multitenant(
        keypair.clone(),
        platform_url.clone(),
        platform_token.clone(),
    )
    .await;
    let matrix_client = substrate_client::setup_client().await;

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
        _ = matrix_tx_processor.start() => {}
        _ = wallet_poller.start() => {}
        _ = wallet_processor.start() => {}
    }

    Ok(())
}
