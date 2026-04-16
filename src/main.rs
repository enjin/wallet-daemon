#![allow(missing_docs, long_running_const_eval, clippy::too_many_arguments)]

use reqwest::header::{AUTHORIZATION, HeaderMap, USER_AGENT};
use std::env;
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use subxt::OfflineClient;

use crate::importer::write_seed;
use crate::multitenant::set_multitenant;
use crate::substrate_client::EnjinConfig;
use crate::transaction::TransactionJob;
use crate::wallet::DeriveWalletJob;
use crate::wallet_loader::load_wallet;

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
    let seed_path = dotenvy::var("SEED_PATH").unwrap_or("store".to_string());
    let seed_path = PathBuf::from_str(&seed_path).expect("SEED_PATH must be a valid path");
    if !seed_path.exists() {
        panic!("SEED_PATH does not exist: {:?}", seed_path)
    };
    let seed_path = if seed_path.is_dir() {
        seed_path.join("wallet.seed")
    } else {
        seed_path
    };

    let args: Vec<String> = env::args().skip(1).collect();
    if let Some(arg) = args.first()
        && arg == "import"
    {
        println!("Enjin Platform - Import Wallet");
        let seed = rpassword::prompt_password("Please type your 12-word mnemonic: ").unwrap();
        let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
        write_seed(seed, &seed_path, &key_pass).expect("Failed to import your wallet");

        exit(1);
    }

    let keypair = {
        let key_pass = dotenvy::var("KEY_PASS").expect("KEY_PASS env var is required");
        load_wallet(&seed_path, &key_pass)
    };

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
            AUTHORIZATION,
            platform_token
                .parse()
                .expect("could not parse Authorization header"),
        );
        headers.insert(
            USER_AGENT,
            format!("Enjin-Wallet-Daemon/{}", env!("CARGO_PKG_VERSION"))
                .parse()
                .expect("could not parse User-Agent header"),
        );
        global::HEADERS
            .set(headers)
            .expect("platform token already set");
    }

    tracing_subscriber::fmt::init();
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
