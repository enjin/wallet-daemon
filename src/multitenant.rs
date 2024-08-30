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

async fn get_packages() -> Result<bool, Box<dyn std::error::Error>> {
    let client = Client::new();
    let res = client
        .get("https://platform.canary.enjin.io/.well-known/enjin-platform.json")
        .send()
        .await?;
    let platform = res.json::<Platform>().await?;

    Ok(platform
        .packages
        .contains_key("enjin/platform-multi-tenant"))
}

async fn update_user(account: String) -> Result<bool, Box<dyn std::error::Error>> {
    let request_body =
        graphql::UpdateUser::build_query(graphql::update_user::Variables { account });

    let client = reqwest::Client::new();
    let res = client
        .post("https://platform.canary.enjin.io/graphql/multi-tenant")
        .header(
            "Authorization",
            "6ZyLaVnIrHUxBTz7vPye3g0mxhvuumrEx7oqbe0w6d07a3a2",
        )
        .json(&request_body)
        .send()
        .await?;

    let result: Response<graphql::update_user::ResponseData> = res.json().await?;
    let data = result.data.expect("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.");

    Ok(data.update_user)
}

pub async fn set_multitenant(account: String) {
    let is_tenant = get_packages()
        .await
        .expect("We could not connect to Enjin Platform, check your connection or the url");

    if is_tenant {
        let updated = update_user(account)
            .await
            .expect("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.");

        println!(
            "** Your account at Enjin Platform has been updated with your wallet daemon address"
        );

        if !updated {
            panic!("You are connected to a multi-tenant platform but the daemon has failed to update your account. Check your access token or if you are connected to the correct platform.")
        }
    }
}
