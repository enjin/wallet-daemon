#![allow(missing_docs)]
#![allow(dead_code)]
#![allow(unused)]

use std::env;
use std::fs::File;
use std::process::exit;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subxt::backend::rpc::reconnecting_rpc_client::{Client, ExponentialBackoff};
use subxt::{OnlineClient, PolkadotConfig};
use subxt_signer::sr25519::Keypair;
use subxt_signer::SecretUri;
use tokio::signal;
use tokio::task::JoinHandle;
use wallet_daemon_new::config_loader::{load_config, load_wallet};
use wallet_daemon_new::{
    set_multitenant, write_seed, BlockSubscription, DeriveWalletJob, TransactionJob,
};

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

    let (keypair, matrix_url, relay_url, platform_url, platform_token) =
        load_wallet(load_config()).await;
    let signing = hex::encode(keypair.public_key().0);

    tracing_subscriber::fmt::init();

    set_multitenant(signing, platform_url.clone(), platform_token.clone()).await;

    let reconnect = Client::builder()
        .retry_policy(
            ExponentialBackoff::from_millis(100)
                .max_delay(Duration::from_secs(10))
                .take(3),
        )
        .build(matrix_url.clone())
        .await
        .unwrap();

    let block_rpc = OnlineClient::<PolkadotConfig>::from_rpc_client(reconnect.clone())
        .await
        .unwrap();

    let sub = BlockSubscription::new(block_rpc);

    let rpc = OnlineClient::<PolkadotConfig>::from_url(matrix_url)
        .await
        .unwrap();

    let (transaction_poller, transaction_processor) = TransactionJob::create_job(
        rpc,
        Arc::clone(&sub),
        keypair.clone(),
        platform_url.clone(),
        platform_token.clone(),
    );

    transaction_poller.start();
    transaction_processor.start();

    //
    // // let (wallet_poller, wallet_processor) =
    // //     DeriveWalletJob::create_job(keypair, platform_url, platform_token);
    //

    // wallet_poller.start();
    // wallet_processor.start();

    signal::ctrl_c().await.expect("Failed to listen for ctrl c");

    Ok(())
}
