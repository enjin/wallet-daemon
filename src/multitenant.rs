use crate::graphql;
use graphql_client::{GraphQLQuery, Response};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use subxt_signer::sr25519::Keypair;

#[allow(dead_code)]
#[derive(Deserialize)]
struct Platform {
    packages: HashMap<String, Value>,
}

#[allow(dead_code)]
async fn get_packages(platform_url: String) -> Result<bool, Box<dyn std::error::Error>> {
    let platform = platform_url.replace("/graphql", "");

    let client = Client::new();
    let res = client
        .get(format!("{platform}/.well-known/enjin-platform.json"))
        .send()
        .await?;
    let platform = res.json::<Platform>().await?;

    Ok(platform
        .packages
        .contains_key("enjin/platform-multi-tenant"))
}

async fn set_daemon_wallet_account(
    keypair: Keypair,
    platform_url: String,
    platform_token: String,
) -> Result<bool, Box<dyn std::error::Error>> {
    let message = b"EnjinPlatform.VerifyDaemonWallet";
    let signature = keypair.sign(message);
    let request_body = graphql::SetDaemonWalletAccount::build_query(
        graphql::set_daemon_wallet_account::Variables {
            public_key: format!("0x{}", hex::encode(keypair.public_key().0)),
            signed_message: format!("0x{}", hex::encode(signature.0)),
        },
    );

    let client = reqwest::Client::new();
    let res = client
        .post(platform_url)
        .header("Authorization", platform_token)
        .json(&request_body)
        .send()
        .await?;

    let result: Response<graphql::set_daemon_wallet_account::ResponseData> = res.json().await?;
    let data = result.data.expect("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.");

    Ok(data.result)
}

pub async fn set_multitenant(keypair: Keypair, platform_url: String, platform_token: String) {
    let account = hex::encode(keypair.public_key().0);
    let updated = set_daemon_wallet_account(keypair, platform_url.clone(), platform_token)
        .await
        .expect("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.");

    let trimmed_url = platform_url
        .trim_end_matches("/graphql")
        .replace("https://", "");
    let trimmed_account = format!("0x{}...{}", &account[..4], &account[60..]);
    println!("** (MultiTenant) Wallet at {trimmed_url} set to: {trimmed_account}");

    if !updated {
        panic!("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.")
    }
}
