//! Wallet daemon for Enjin Matrixchain
//!
//! It polls for transactions from Enjin Platform
//! signs them and sends them to the blockchain.
//!
//! A configuration file is needed(See README for specifics)

use sp_core::crypto::Ss58Codec;
use sp_core::{sr25519, Pair};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::{sync::Arc, time::Duration};

use graphql_client::{GraphQLQuery, Response};
use reqwest;
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::error::Error;
use std::process::exit;
use subxt::DefaultConfig;
use tokio::signal;
use wallet_lib::wallet_trait::Wallet;
use wallet_lib::*;

/// Time in milliseconds that a block is expected to be generated.
const POLL_TRANSACTION_MS: u64 = 12000;
const POLL_ACCOUNT_MS: u64 = 2500;

/// Page size for the number of transaction requests that are brought with each polling.
const TRANSACTION_PAGE_SIZE: usize = 50;
const ACCOUNT_PAGE_SIZE: usize = 200;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql/schema.graphql",
    query_path = "src/graphql/update_user.graphql",
    response_derives = "Debug"
)]
pub struct UpdateUser;

#[derive(Deserialize)]
struct Platform {
    packages: HashMap<String, Value>,
}

async fn get_packages(graphql_endpoint: String) -> Result<bool, Box<dyn Error>> {
    let url = graphql_endpoint.replace("graphql", ".well-known/enjin-platform.json");
    let client = reqwest::Client::new();

    let res = client.get(url).send().await?;
    let platform = res.json::<Platform>().await.expect("Failed");

    Ok(platform
        .packages
        .contains_key("enjin/platform-multi-tenant"))
}

async fn update_user(
    graphql_endpoint: String,
    token: String,
    account: String,
) -> Result<bool, Box<dyn Error>> {
    let request_body = UpdateUser::build_query(update_user::Variables { account });

    let client = reqwest::Client::new();
    let res = client
        .post(format!("{}{}", graphql_endpoint, "/multi-tenant"))
        .header("Authorization", &*token)
        .json(&request_body)
        .send()
        .await?;

    let result: Response<update_user::ResponseData> = res.json().await?;
    let data = result.data.expect("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.");

    Ok(data.update_user)
}

fn write_seed(seed: String) -> std::io::Result<()> {
    let split_seed: Vec<String> = seed.split("///").map(|s| s.to_string()).collect();
    let mnemonic = split_seed[0].clone();
    let password = split_seed
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<String>>()
        .join(" ");

    let key = sr25519::Pair::from_phrase(&mnemonic, Some(&*password))
        .expect("Invalid mnemonic phrase or password");

    let public_key = key.0.public();
    let address = public_key.to_ss58check();
    let hex_key = hex::encode(public_key);
    let file_name = format!("73723235{}", hex_key);

    println!("Public Key (Hex): 0x{}", hex_key.clone());
    println!("Address (SS58): {}", address);
    println!("Store file name: {}", file_name);

    let mut file = File::create(format!("store/{}", file_name))?;
    file.write_all(format!("\"{}\"", mnemonic).as_bytes())?;

    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if let Some(arg) = args.first() {
        if arg == "import" {
            println!("Enjin Platform - Import Wallet");
            let seed = rpassword::prompt_password("Please type your 12-word mnemonic: ").unwrap();
            write_seed(seed).expect("Failed to import your wallet");
            exit(0)
        }
    }

    tracing_subscriber::fmt::init();

    let (wallet_pair, graphql_endpoint, token) = load_wallet::<DefaultConfig>(load_config()).await;
    let public_key = wallet_pair
        .wallet
        .account_id()
        .await
        .expect("We have failed to decode your daemon public key");

    let is_tenant = get_packages(graphql_endpoint.clone())
        .await
        .expect("We could not connect to Enjin Platform, check your connection or the url");

    if is_tenant {
        let updated = update_user(graphql_endpoint.clone(), token.clone(), public_key.to_string())
            .await
            .expect("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.");

        println!(
            "** Your account at Enjin Platform has been updated with your wallet daemon address"
        );

        if !updated {
            panic!("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.")
        }
    }

    let wallet_pair = Arc::new(wallet_pair);

    let (poll_job, sign_processor) = create_job_pair(
        graphql_endpoint.clone(),
        String::from(token.clone()),
        Duration::from_millis(POLL_TRANSACTION_MS),
        Arc::clone(&wallet_pair),
        TRANSACTION_PAGE_SIZE.try_into().unwrap(),
    );

    let (poll_wallet_job, derive_wallet_processor) = create_wallet_job_pair(
        graphql_endpoint,
        String::from(token.clone()),
        Duration::from_millis(POLL_ACCOUNT_MS),
        Arc::clone(&wallet_pair),
        ACCOUNT_PAGE_SIZE.try_into().unwrap(),
    );

    poll_job.start_job();
    sign_processor.start_job();

    poll_wallet_job.start_job();
    derive_wallet_processor.start_job();

    signal::ctrl_c().await.expect("Failed to listen for ctrl c");
}
