#![allow(missing_docs, long_running_const_eval, clippy::too_many_arguments)]

use crate::multitenant::set_multitenant;
use crate::substrate_client::EnjinConfig;
use crate::transaction::TransactionJob;
use crate::types::{Cli, Commands};
use crate::wallet::DeriveWalletJob;
use crate::wallet_loader::{load_seed, resolve_seed_path};
use crate::websocket::PusherConnection;
use crate::work_trigger::{PusherStatus, WorkTrigger};
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
mod env_loader;
mod global;
mod graphql;
mod importer;
mod multitenant;
mod platform_client;
mod retry;
pub mod substrate_client;
mod transaction;
mod types;
mod utils;
mod wallet;
mod wallet_loader;
mod websocket;
mod work_trigger;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load `.env` (transcoding UTF-16 to UTF-8) before any `dotenvy::var` call.
    env_loader::load_env();

    // init logging before anything that can emit a warning. `load_seed` warns
    // when it is about to generate a NEW wallet identity, and subcommands such
    // as `print-seed` call it, so a subscriber installed after the subcommand
    // match would silently swallow exactly the warning that matters most.
    let log_filter = dotenvy::var("RUST_LOG").unwrap_or("wallet_daemon=info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(log_filter))
        .finish()
        .try_init()?;

    let seed_path = resolve_seed_path(dotenvy::var("SEED_PATH").ok().as_deref());

    // check for subcommands
    match Cli::parse().command {
        Some(Commands::Import) => {
            println!("Enjin Platform - Import Wallet");
            if seed_path.is_file() && seed_path.exists() {
                panic!("importing wallet would overwrite existing file at {seed_path:?}");
                // TODO: it can also panic if it's a directory. Consider refactoring to also allow
                // panicking here in that case. Currently, it will panic after password is entered.
            }
            let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
            let seed = rpassword::prompt_password("Please type your 12-word mnemonic: ").unwrap();
            importer::write_seed(seed, &seed_path, &key_pass)
                .expect("Failed to import your wallet");

            exit(0);
        }
        Some(Commands::PrintSeed) => {
            let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
            load_seed(seed_path.clone(), &key_pass, true);
            exit(0);
        }
        None => {
            // if there is no subcommand, run the daemon
        }
    }

    let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
    let keypair = load_seed(seed_path, &key_pass, false);

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

    let transaction_trigger = WorkTrigger::new();
    let wallet_trigger = WorkTrigger::new();
    let pusher_status = PusherStatus::new();

    let (matrix_tx_poller, matrix_tx_processor) = TransactionJob::create_job(
        keypair.clone(),
        transaction_trigger.clone(),
        pusher_status.clone(),
    );

    let (wallet_poller, wallet_processor) =
        DeriveWalletJob::create_job(keypair, wallet_trigger.clone(), pusher_status.clone());
    let websocket = PusherConnection::from_env(transaction_trigger, wallet_trigger, pusher_status)?;

    // Every one of these tasks is supposed to run for the life of the process.
    // If any of them completes — returning, or panicking and resolving the
    // `JoinHandle` to `Err(JoinError)` — the daemon has lost a component it
    // cannot rebuild, and in-flight batches are gone. Exit non-zero so a
    // supervisor (systemd `Restart=on-failure`, ECS, Docker) actually restarts
    // it; returning `Ok(())` here would look like a clean shutdown and leave a
    // dead signer in place.
    let (task, result) = tokio::select! {
        result = websocket.start() => ("pusher websocket", result),
        result = matrix_tx_poller.start() => ("transaction poller", result),
        result = matrix_tx_processor.start() => ("transaction processor", result),
        result = wallet_poller.start() => ("managed-wallet poller", result),
        result = wallet_processor.start() => ("managed-wallet processor", result),
    };

    match result {
        Ok(()) => tracing::error!("The {task} task exited unexpectedly; shutting down"),
        Err(error) => tracing::error!("The {task} task terminated abnormally: {error}"),
    }

    exit(1);
}
