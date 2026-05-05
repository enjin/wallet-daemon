#![allow(missing_docs, long_running_const_eval, clippy::too_many_arguments)]

use crate::multitenant::set_multitenant;
use crate::substrate_client::EnjinConfig;
use crate::transaction::TransactionJob;
use crate::types::{Cli, Commands};
use crate::wallet::DeriveWalletJob;
use crate::wallet_loader::{load_seed, resolve_seed_path};
use clap::Parser;
use reqwest::header::HeaderMap;
use std::env;
use std::process::exit;
use subxt::OfflineClient;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;

pub type SubstrateClient = OfflineClient<EnjinConfig>;

pub const DEFAULT_PLATFORM_URL: &str = "https://platform.enjin.io/graphql/daemon";
pub const TX_MORTALITY: u64 = 64;
pub const DUMMY_TX_MORTALITY: u64 = 14_400;

mod chain_info;
mod crypto;
mod global;
mod graphql;
mod importer;
mod multitenant;
mod platform_client;
pub mod substrate_client;
mod transaction;
mod types;
mod utils;
mod wallet;
mod wallet_loader;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seed_path_buf = resolve_seed_path(dotenvy::var("SEED_PATH").ok().as_deref());
    let seed_path = seed_path_buf.to_string_lossy().to_string();

    // check for subcommands
    match Cli::parse().command {
        Some(Commands::Import) => {
            println!("Enjin Platform - Import Wallet");
            let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
            let seed = rpassword::prompt_password("Please type your 12-word mnemonic: ").unwrap();
            importer::write_seed(seed, &seed_path_buf, &key_pass)
                .expect("Failed to import your wallet");

            exit(0);
        }
        Some(Commands::PrintSeed) => {
            let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
            load_seed(&seed_path, &key_pass, true);
            exit(0);
        }
        None => {
            // if there is no subcommand, run the daemon
        }
    }

    // init logging
    let log_filter = dotenvy::var("RUST_LOG").unwrap_or("wallet_daemon=info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(log_filter))
        .finish()
        .try_init()?;

    let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
    let keypair = load_seed(&seed_path, &key_pass, false);

    let platform_url = dotenvy::var("PLATFORM_URL").unwrap_or(DEFAULT_PLATFORM_URL.to_string());
    global::PLATFORM_URL
        .set(platform_url.clone())
        .expect("platform url already set");
    println!("** Platform URL: {}", platform_url);
    println!("*****************************************************************");

    // set up headers
    {
        let platform_key = dotenvy::var("PLATFORM_KEY").expect("PLATFORM_KEY env var is required");
        let platform_token = format!("Bearer {}", platform_key);

        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            platform_token
                .parse()
                .expect("could not parse Authorization header"),
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            format!("Enjin-Wallet-Daemon/{}", env!("CARGO_PKG_VERSION"))
                .parse()
                .expect("could not parse User-Agent header"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            "application/json"
                .parse()
                .expect("could not parse Accept header"),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json"
                .parse()
                .expect("could not parse content-type header"),
        );
        global::HEADERS.set(headers).expect("headers already set");
    }

    set_multitenant(keypair.clone()).await;

    let (matrix_tx_poller, matrix_tx_processor) = TransactionJob::create_job(keypair.clone());

    let (wallet_poller, wallet_processor) = DeriveWalletJob::create_job(keypair);

    tokio::select! {
        _ = matrix_tx_poller.start() => {}
        _ = matrix_tx_processor.start() => {}
        _ = wallet_poller.start() => {}
        _ = wallet_processor.start() => {}
    }

    Ok(())
}
