use crate::graphql::populate_managed_wallets::PopulateManagedWalletInput;
use crate::graphql::sign_transactions::SignTransactionInput;
use crate::graphql::{
    AuthenticatePusherSocket, PopulateManagedWallets, SignTransactions, authenticate_pusher_socket,
    populate_managed_wallets, sign_transactions,
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

#[derive(Debug)]
pub struct PusherSubscription {
    pub auth: String,
    pub channel: String,
}

pub async fn authenticate_pusher_socket(
    socket_id: String,
) -> Result<PusherSubscription, Box<dyn std::error::Error + Send + Sync>> {
    let response = utils::execute_query_redacted::<AuthenticatePusherSocket>(
        authenticate_pusher_socket::Variables { id: socket_id },
        None,
    )
    .await?;

    if tracing::enabled!(tracing::Level::DEBUG) {
        let redacted_response = serde_json::json!({
            "data": {
                "result": {
                    "auth": redact_auth(&response.result.auth),
                    "channel": &response.result.channel,
                }
            }
        });
        tracing::debug!("Response Body: {redacted_response}");
    }

    Ok(PusherSubscription {
        auth: response.result.auth,
        channel: response.result.channel,
    })
}

fn redact_auth(auth: &str) -> String {
    "*".repeat(auth.chars().count())
}

pub async fn sign_transactions(
    transactions: Vec<SignTransactionInput>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let uuids: Vec<_> = transactions.iter().map(|x| x.uuid.clone()).collect();

    let res = utils::execute_query::<SignTransactions>(
        sign_transactions::Variables { transactions },
        Some(PlatformExponentialBuilder::default()),
    )
    .await;

    match res {
        Ok(r) => {
            tracing::debug!("Response from platform: {:?}", r);
            tracing::info!(
                "SignTransactions: {} transaction(s) submitted to platform successfully",
                uuids.len(),
            );
            for uuid in uuids {
                tracing::debug!("Signed transaction #{}", uuid);
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

pub async fn populate_managed_wallets(
    wallets: Vec<PopulateManagedWalletInput>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            tracing::debug!("Response from platform: {:?}", r);
            // The mutation returns `Boolean!`. A `false` is a business-level
            // rejection, not a transport error: treating it as success would
            // advance the cursor and log "Updated wallet" lines for wallets the
            // platform did not populate, leaving the rows to reappear on every
            // subsequent scan with nothing in the logs to explain it.
            if !r.result {
                return Err(format!(
                    "Platform rejected PopulateManagedWallets for {} wallet(s): {:?}",
                    external_ids_and_accounts.len(),
                    external_ids_and_accounts
                        .iter()
                        .map(|(external_id, _)| external_id.as_str())
                        .collect::<Vec<_>>(),
                )
                .into());
            }
            for (external_id, account) in external_ids_and_accounts {
                tracing::info!("Updated wallet (externalId: {external_id}) to {account}");
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_redaction_preserves_character_count_without_exposing_content() {
        let auth = "8ab7ab8c519e8f59b635:secret-signature";
        let redacted = redact_auth(auth);

        assert_eq!(redacted.chars().count(), auth.chars().count());
        assert!(redacted.chars().all(|character| character == '*'));
    }

    #[test]
    fn empty_auth_redacts_to_an_empty_string() {
        assert_eq!(redact_auth(""), "");
    }
}
