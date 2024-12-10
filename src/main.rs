#![allow(missing_docs)]
use std::env;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;
use subxt::backend::rpc::reconnecting_rpc_client::{ExponentialBackoff, RpcClient};
use subxt::{OnlineClient, PolkadotConfig};
use wallet_daemon::config_loader::{load_config, load_wallet};
use wallet_daemon::{
    set_multitenant, write_seed, DeriveWalletJob, SubscriptionJob, SubscriptionParams,
    TransactionJob,
};

async fn setup_client(
    url: &str,
) -> (
    Arc<OnlineClient<PolkadotConfig>>,
    SubscriptionJob,
    Arc<SubscriptionParams>,
) {
    let rpc_client = RpcClient::builder()
        .retry_policy(
            ExponentialBackoff::from_millis(100)
                .max_delay(Duration::from_secs(10))
                .take(3),
        )
        .build(url.to_string())
        .await
        .unwrap();

    let online_client = OnlineClient::<PolkadotConfig>::from_rpc_client(rpc_client)
        .await
        .unwrap();

    let runtime_updater = online_client.updater();
    tokio::spawn(async move {
        let _ = runtime_updater.perform_runtime_updates().await;
    });

    let client = Arc::new(online_client);
    let subscription = SubscriptionJob::create_job(Arc::clone(&client));
    let sub_params = subscription.get_params();

    (client, subscription, sub_params)
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

    let (keypair, matrix_url, relay_url, platform_url, platform_token) =
        load_wallet(load_config()).await;
    let signing = hex::encode(keypair.public_key().0);

    tracing_subscriber::fmt::init();
    // Check if we are connecting to a multitenant platform
    set_multitenant(signing, platform_url.clone(), platform_token.clone()).await;
    // Setup matrix client and parameters
    let (matrix_client, matrix_subscription, matrix_sub_params) = setup_client(&matrix_url).await;
    // Setup relay client and parameters
    let (relay_client, relay_subscription, relay_sub_params) = setup_client(&relay_url).await;

    let (matrix_tx_poller, matrix_tx_processor) = TransactionJob::create_job(
        Arc::clone(&matrix_client),
        Arc::clone(&matrix_sub_params),
        keypair.clone(),
        platform_url.clone(),
        platform_token.clone(),
    );

    let (relay_tx_poller, relay_tx_processor) = TransactionJob::create_job(
        Arc::clone(&relay_client),
        Arc::clone(&relay_sub_params),
        keypair.clone(),
        platform_url.clone(),
        platform_token.clone(),
    );

    let (wallet_poller, wallet_processor) =
        DeriveWalletJob::create_job(keypair, platform_url, platform_token);

    tokio::select! {
        r = relay_subscription.start() => {
            let err = r.unwrap_err();
            tracing::error!("Subscription job failed: {:?}", err);
        }
        m = matrix_subscription.start() => {
            let err = m.unwrap_err();
            tracing::error!("Subscription job failed: {:?}", err);
        }

        _ = relay_tx_poller.start() => {}
        _ =  relay_tx_processor.start() => {}
        _ = matrix_tx_poller.start() => {}
        _ =  matrix_tx_processor.start() => {}
        _ = wallet_poller.start() => {}
        _ = wallet_processor.start() => {}
    }

    Ok(())
}
