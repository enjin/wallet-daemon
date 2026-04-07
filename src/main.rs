#![allow(missing_docs)]
use parity_scale_codec::Decode;
use std::env;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;
use subxt::client::RuntimeVersion;
use subxt::metadata::types::Metadata;
use subxt::{OfflineClient, PolkadotConfig};
use wallet_daemon::config_loader::{load_config, load_wallet};
use wallet_daemon::{set_multitenant, write_seed, DeriveWalletJob, TransactionJob};

async fn setup_client(url: &str) -> Arc<OfflineClient<PolkadotConfig>> {
    let online_client = OfflineClient::<PolkadotConfig>::new(
        Default::default(),
        RuntimeVersion {
            spec_version: 0,
            transaction_version: 0,
        },
        Metadata::decode(&mut &[0_u8; 32][..]).unwrap(),
    );

    let client = Arc::new(online_client);

    client
}

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
    let matrix_client = setup_client(&matrix_url).await;

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
