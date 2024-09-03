#![allow(dead_code)]
#![allow(unused)]

use crate::graphql;
use graphql_client::{GraphQLQuery, Response};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Deserialize)]
struct Platform {
    packages: HashMap<String, Value>,
}

async fn get_packages(platform_url: String, platform_token: String) -> Result<bool, Box<dyn std::error::Error>> {
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

async fn update_user(account: String, platform_url: String, platform_token: String) -> Result<bool, Box<dyn std::error::Error>> {
    let request_body =
        graphql::UpdateUser::build_query(graphql::update_user::Variables { account });

    let client = reqwest::Client::new();
    let res = client
        .post(format!("{platform_url}/multi-tenant"))
        .header(
            "Authorization",
            platform_token,
        )
        .json(&request_body)
        .send()
        .await?;

    let result: Response<graphql::update_user::ResponseData> = res.json().await?;
    let data = result.data.expect("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.");

    Ok(data.update_user)
}

pub async fn set_multitenant(account: String, platform_url: String, platform_token: String) {
    let is_tenant = get_packages(platform_url.clone(), platform_token.clone())
        .await
        .expect("We could not connect to Enjin Platform, check your connection or the url");

    if is_tenant {
        let updated = update_user(account.clone(), platform_url.clone(), platform_token)
            .await
            .expect("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.");

        let trimmed_url = platform_url.trim_end_matches("/graphql").replace("https://", "");
        let trimmed_account = format!("0x{}...{}", &account[..4], &account[60..]);
        println!(
            "** (MultiTenant) Wallet at {trimmed_url} set to: {trimmed_account}"
        );

        if !updated {
            panic!("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.")
        }
    }
}
