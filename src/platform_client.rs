use crate::graphql::populate_managed_wallets::PopulateManagedWalletInput;
use crate::graphql::sign_transactions::SignTransactionInput;
use crate::graphql::{
    PopulateManagedWallets, SignTransactions, populate_managed_wallets, sign_transactions,
};
use crate::utils;
use backon::ExponentialBuilder;
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

pub async fn sign_transactions(transactions: Vec<SignTransactionInput>) {
    let uuids: Vec<_> = transactions.iter().map(|x| x.uuid.clone()).collect();

    let res = utils::execute_query::<SignTransactions>(
        sign_transactions::Variables { transactions },
        Some(PlatformExponentialBuilder::default()),
    )
    .await;

    match res {
        Ok(r) => {
            tracing::info!("Response from platform: {:?}", r);
            for uuid in uuids {
                tracing::info!("Signed transaction #{}", uuid);
            }
        }
        Err(e) => tracing::error!("Error sending SignTransactions: {:?}", e),
    }
}

pub async fn populate_managed_wallets(wallets: Vec<PopulateManagedWalletInput>) {
    let external_ids_and_accounts: Vec<(String, String)> = wallets
        .iter()
        .map(|x| (x.external_id.clone(), x.public_key.clone()))
        .collect();

    let res = utils::execute_query::<PopulateManagedWallets>(
        populate_managed_wallets::Variables { wallets },
        Some(PlatformExponentialBuilder::default()),
    )
    .await;

    match res {
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
    }
}
