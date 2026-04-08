#![allow(missing_docs, long_running_const_eval)]

use hex_literal::hex;
use parity_scale_codec::Decode;
use std::env;
use std::process::exit;
use std::sync::Arc;
use subxt::config::polkadot::SpecVersionForRange;
use subxt::{config::PolkadotConfig, Config, Metadata, OfflineClient, SubstrateConfig};
use subxt::utils::H256;
use wallet_daemon::config_loader::{load_config, load_wallet};
use wallet_daemon::{set_multitenant, write_seed, DeriveWalletJob, SubstrateClient, TransactionJob};

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
    // Check if we are connecting to a multitenant platform
    set_multitenant(
        keypair.clone(),
        platform_url.clone(),
        platform_token.clone(),
    )
    .await;
    // Setup matrix client and parameters
    // TODO: metadata will need to be updated when it changes
    let matrix_client = wallet_daemon::substrate_client::setup_client(&matrix_url).await;

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
