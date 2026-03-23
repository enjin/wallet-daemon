use crate::graphql;
use graphql_client::{GraphQLQuery, Response};
use subxt_signer::sr25519::Keypair;

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
    let data = result.data.expect("There was an error updating your account. Please check your access token.");

    Ok(data.result)
}

pub async fn set_multitenant(keypair: Keypair, platform_url: String, platform_token: String) {
    let account = hex::encode(keypair.public_key().0);
    let updated = set_daemon_wallet_account(keypair, platform_url.clone(), platform_token)
        .await
        .expect("There was an error updating your account. Please check your access token.");

    tracing::info!("Platform wallet daemon set to: {account}");

    if !updated {
        panic!("There was an error updating your account. Please check your access token.")
    }
}
