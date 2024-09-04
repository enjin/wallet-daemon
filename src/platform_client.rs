#![allow(dead_code)]
#![allow(unused)]

use crate::graphql::{set_wallet_account, update_transaction, SetWalletAccount, UpdateTransaction};
use backon::{BlockingRetryable, ExponentialBuilder, Retryable};
use graphql_client::GraphQLQuery;
use reqwest::Client;
use std::time::Duration;

pub struct PlatformExponentialBuilder(ExponentialBuilder);
impl PlatformExponentialBuilder {
    pub fn default() -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_jitter()
            .with_factor(1.5)
            .with_min_delay(Duration::from_secs(6))
            .with_max_delay(Duration::from_secs(40))
            .with_max_times(6)
    }
}

pub struct Transaction {
    pub(crate) id: i64,
    pub(crate) state: String,
    pub(crate) hash: Option<String>,
    pub(crate) signer: Option<String>,
    pub(crate) signed_at: Option<i64>,
}

pub async fn update_transaction(
    client: Client,
    platform_url: String,
    platform_token: String,
    transaction: Transaction,
) {
    let transaction_state = match transaction.state.as_str() {
        "EXECUTED" => update_transaction::TransactionState::EXECUTED,
        "BROADCAST" => update_transaction::TransactionState::BROADCAST,
        _ => update_transaction::TransactionState::ABANDONED,
    };

    let request_body = UpdateTransaction::build_query(update_transaction::Variables {
        id: transaction.id,
        state: Some(transaction_state),
        transaction_hash: transaction.hash,
        signing_account: transaction.signer,
        signed_at_block: transaction.signed_at,
    });

    let res = (|| async {
        client
            .post(&platform_url)
            .header("Authorization", &platform_token)
            .json(&request_body)
            .send()
            .await
    })
    .retry(PlatformExponentialBuilder::default())
    .await;

    match res {
        Ok(res) => match res
            .json::<graphql_client::Response<update_transaction::ResponseData>>()
            .await
        {
            Ok(r) => {
                tracing::info!("Response from platform: {:?}", r);
                tracing::info!(
                    "Updated transaction #{} with state: {}",
                    transaction.id,
                    transaction.state,
                );
            }
            Err(e) => tracing::error!("Error decoding response of the platform: {:?}", e),
        },
        Err(e) => tracing::error!("Error sending UpdateTransaction: {:?}", e),
    }
}

pub async fn set_wallet_account(
    client: Client,
    platform_url: String,
    platform_token: String,
    wallet_id: i64,
    external_id: String,
    account: String,
) {
    let request_body = SetWalletAccount::build_query(set_wallet_account::Variables {
        id: wallet_id,
        account: account.clone(),
    });

    let res = (|| async {
        client
            .post(&platform_url)
            .header("Authorization", &platform_token)
            .json(&request_body)
            .send()
            .await
    })
    .retry(PlatformExponentialBuilder::default())
    .await;

    match res {
        Ok(res) => match res
            .json::<graphql_client::Response<set_wallet_account::ResponseData>>()
            .await
        {
            Ok(r) => {
                tracing::info!("Response from platform: {:?}", r);
                tracing::info!(
                    "Updated wallet {wallet_id} (externalId: {external_id}) to {account}"
                );
            }
            Err(e) => tracing::error!(
                "Error decoding body {:?} of response to submitted account",
                e
            ),
        },
        Err(e) => tracing::error!(
            "Error decoding body {:?} of response to submitted account",
            e
        ),
    }
}
