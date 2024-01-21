//! Wallet daemon for efinity blockchain
//!
//! It polls for transactions, signs them and sends them to the blockchain.
//!
//! A configuration file is needed(See README for specifics)

use std::collections::HashMap;
use std::{sync::Arc, time::Duration};

use graphql_client::{GraphQLQuery, Response};
use reqwest;
use serde::Deserialize;
use serde_json::Value;
use std::error::Error;
use subxt::DefaultConfig;
use tokio::signal;
use wallet_lib::wallet_trait::Wallet;
use wallet_lib::*;

/// Time in milliseconds that a block is expected to be generated.
const BLOCKTIME_MS: u64 = 12000;

/// Page size for the number of transaction requests that are brought with each polling.
const PAGE_SIZE: usize = 40;

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
    let data = result.data.expect("Missing response");

    Ok(data.update_user)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let (wallet_pair, graphql_endpoint, token) = load_wallet::<DefaultConfig>(load_config()).await;

    let public_key = wallet_pair
        .wallet
        .account_id()
        .await
        .expect("Failed to decode daemon public key");

    let is_tenant = get_packages(graphql_endpoint.clone())
        .await
        .expect("Failed to connect with Enjin Platform");

    if is_tenant {
        let updated = update_user(graphql_endpoint.clone(), token.clone(), public_key.to_string())
            .await
            .expect("You are connected to a multi-tenant platform but the daemon failed to update your user");

        println!("** Updated your wallet daemon address at Enjin Platform **");

        if !updated {
            panic!("You are connected to a multi-tenant platform but the daemon failed to update your user")
        }
    }

    let wallet_pair = Arc::new(wallet_pair);

    let (poll_job, sign_processor) = create_job_pair(
        graphql_endpoint.clone(),
        String::from(token.clone()),
        Duration::from_millis(BLOCKTIME_MS),
        Arc::clone(&wallet_pair),
        PAGE_SIZE.try_into().unwrap(),
    );

    let (poll_wallet_job, derive_wallet_processor) = create_wallet_job_pair(
        graphql_endpoint,
        String::from(token.clone()),
        Duration::from_millis(BLOCKTIME_MS),
        Arc::clone(&wallet_pair),
        PAGE_SIZE.try_into().unwrap(),
    );

    poll_job.start_job();
    sign_processor.start_job();

    poll_wallet_job.start_job();
    derive_wallet_processor.start_job();

    signal::ctrl_c().await.expect("Failed to listen for ctrl c");
}
