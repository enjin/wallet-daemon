use crate::graphql::SetDaemonWalletAccount;
use crate::{graphql, utils};
use subxt_signer::sr25519::Keypair;

async fn set_daemon_wallet_account(
    keypair: Keypair,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let message = b"EnjinPlatform.VerifyDaemonWallet";
    let signature = keypair.sign(message);
    let result = utils::execute_query::<SetDaemonWalletAccount>(
        graphql::set_daemon_wallet_account::Variables {
            public_key: format!("0x{}", hex::encode(keypair.public_key().0)),
            signed_message: format!("0x{}", hex::encode(signature.0)),
        },
        None,
    )
    .await?;
    Ok(result.result)
}

pub async fn set_multitenant(keypair: Keypair) {
    let account = hex::encode(keypair.public_key().0);
    let updated = set_daemon_wallet_account(keypair)
        .await
        .expect("There was an error updating your account");

    tracing::info!("Platform wallet daemon set to: 0x{account}");

    if !updated {
        panic!(
            "SetDaemonWalletAccount returned false when true was expected. Please check your access token."
        )
    }
}
