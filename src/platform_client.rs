use crate::graphql::populate_managed_wallets::PopulateManagedWalletInput;
use crate::graphql::sign_transactions::SignTransactionInput;
use crate::graphql::{
    populate_managed_wallets, sign_transactions, PopulateManagedWallets, SignTransactions,
};
use backon::{ExponentialBuilder, Retryable};
use graphql_client::GraphQLQuery;
use reqwest::Client;
use std::time::Duration;

pub struct PlatformExponentialBuilder();
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
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) hash: Option<String>,
    pub(crate) signer: Option<String>,
    pub(crate) signed_at: Option<i64>,
}

pub async fn sign_transactions(
    client: Client,
    platform_url: String,
    platform_token: String,
    transaction: SignTransactionInput,
) {
    // let transaction_state = match transaction.state.as_str() {
    //     "EXECUTED" => sign_transactions::TransactionState::EXECUTED,
    //     "BROADCAST" => sign_transactions::TransactionState::BROADCAST,
    //     _ => sign_transactions::TransactionState::ABANDONED,
    // };
    let uuid = transaction.uuid.clone();

    let request_body = SignTransactions::build_query(sign_transactions::Variables {
        transactions: vec![transaction],
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
            .json::<graphql_client::Response<sign_transactions::ResponseData>>()
            .await
        {
            Ok(r) => {
                tracing::info!("Response from platform: {:?}", r);
                tracing::info!("Signed transaction #{}", uuid,);
            }
            Err(e) => {
                tracing::error!("Error decoding response of the platform: {:?}", e);
            }
        },
        Err(e) => tracing::error!("Error sending UpdateTransaction: {:?}", e),
    }
}

pub async fn populate_managed_wallets(
    client: Client,
    platform_url: String,
    platform_token: String,
    wallets: Vec<PopulateManagedWalletInput>,
) {
    let external_ids_and_accounts: Vec<(String, String)> = wallets
        .iter()
        .map(|x| (x.external_id.clone(), x.public_key.clone()))
        .collect();

    let request_body =
        PopulateManagedWallets::build_query(populate_managed_wallets::Variables { wallets });

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
            .json::<graphql_client::Response<populate_managed_wallets::ResponseData>>()
            .await
        {
            Ok(r) => {
                tracing::info!("Response from platform: {:?}", r);
                for (external_id, account) in external_ids_and_accounts {
                    tracing::info!("Updated wallet (externalId: {external_id}) to {account}");
                }
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
