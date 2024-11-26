#![allow(missing_docs)]
use std::env;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;
use subxt::backend::rpc::reconnecting_rpc_client::{ExponentialBackoff, RpcClient};
use subxt::{OnlineClient, PolkadotConfig};
use wallet_daemon::config_loader::{load_config, load_wallet};
use wallet_daemon::{
    set_multitenant, write_seed, DeriveWalletJob, SubscriptionJob, TransactionJob,
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

    let (keypair, matrix_url, _relay_url, platform_url, platform_token) =
        load_wallet(load_config()).await;
    let signing = hex::encode(keypair.public_key().0);

    tracing_subscriber::fmt::init();

    set_multitenant(signing, platform_url.clone(), platform_token.clone()).await;

    let rpc_client = RpcClient::builder()
        .retry_policy(
            ExponentialBackoff::from_millis(100)
                .max_delay(Duration::from_secs(10))
                .take(3),
        )
        .build(matrix_url.clone())
        .await
        .unwrap();

    let chain_client = OnlineClient::<PolkadotConfig>::from_rpc_client(rpc_client)
        .await
        .unwrap();

    let update_task = chain_client.updater();
    tokio::spawn(async move {
        let _ = update_task.perform_runtime_updates().await;
    });

    let chain_client = Arc::new(chain_client);
    let subscription_job = SubscriptionJob::create_job(Arc::clone(&chain_client));
    let params = subscription_job.get_params();

    let (transaction_poller, transaction_processor) = TransactionJob::create_job(
        Arc::clone(&chain_client),
        Arc::clone(&params),
        keypair.clone(),
        platform_url.clone(),
        platform_token.clone(),
    );

    let (wallet_poller, wallet_processor) =
        DeriveWalletJob::create_job(keypair, platform_url, platform_token);

    tokio::select! {
        _ = transaction_poller.start() => {}
        _ =  transaction_processor.start() => {}
        _ = wallet_poller.start() => {}
        _ = wallet_processor.start() => {}
        r = subscription_job.start() => {
            let err = r.unwrap_err();
            tracing::error!("Subscription job failed: {:?}", err);
        }
    }

    Ok(())
}
