use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use std::path::Path;
use std::str::FromStr;
use std::{env, fs};
use subxt_signer::bip39::Mnemonic;
use subxt_signer::sr25519::Keypair;
use subxt_signer::{ExposeSecret, SecretString, SecretUri};

async fn get_keys(key_store_path: &Path, password: SecretString) -> Keypair {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(key_store_path);

    if let Ok(entries) = fs::read_dir(&p) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().unwrap().len() != 72 {
                continue;
            }

            let content = fs::read_to_string(entry.path()).expect("Unable to read file");
            let strip_content = content
                .strip_suffix("\r\n")
                .or(content.strip_suffix("\n"))
                .unwrap_or(&*content)
                .to_string();
            let secret = format!(
                "{}///{}",
                strip_content.replace("\"", ""),
                password.expose_secret()
            );

            let uri = SecretUri::from_str(&secret).expect("valid URI");
            let keypair_tx = Keypair::from_uri(&uri).expect("valid keypair");

            if entry.file_name().to_str().unwrap()
                == format!("73723235{}", hex::encode(keypair_tx.public_key().0))
            {
                return keypair_tx;
            }

            panic!("Key checksum does not match");
        }
    }

    let mnemonic = Mnemonic::generate(12).unwrap().to_string();
    let secret = format!("{}///{}", mnemonic, password.expose_secret());
    let uri = SecretUri::from_str(&secret).expect("valid URI");
    let keypair_tx = Keypair::from_uri(&uri).expect("valid keypair");

    fs::write(
        p.join(format!(
            "73723235{}",
            hex::encode(keypair_tx.public_key().0)
        )),
        format!("\"{}\"", mnemonic),
    )
    .expect("Unable to write file");

    keypair_tx
}

pub async fn load_wallet(master_key: &Path, key_pass: &str) -> Keypair {
    let version = env!("CARGO_PKG_VERSION");
    let password = SecretString::from(key_pass);
    let signer = get_keys(master_key, password).await;
    let public_key = signer.public_key().0;
    let account_id = sp_core::crypto::AccountId32::from(public_key);

    println!("******************* Enjin Wallet Daemon v{version} *******************");
    println!(
        "** Enjin Matrixchain  (SS58): {}",
        account_id.to_ss58check_with_version(Ss58AddressFormat::custom(1110))
    );
    println!(
        "** Canary Matrixchain (SS58): {}",
        account_id.to_ss58check_with_version(Ss58AddressFormat::custom(9030))
    );
    println!(
        "** Public Key          (Hex): 0x{}",
        hex::encode(public_key)
    );

    signer
}

pub enum Chain {
    Matrix,
    Relay,
}

impl From<Chain> for crate::graphql::get_pending_transactions::Chain {
    fn from(value: Chain) -> Self {
        match value {
            Chain::Matrix => Self::MATRIX,
            Chain::Relay => Self::RELAY,
        }
    }
}

impl From<Chain> for crate::graphql::get_pending_managed_wallet_creations::Chain {
    fn from(value: Chain) -> Self {
        match value {
            Chain::Matrix => Self::MATRIX,
            Chain::Relay => Self::RELAY,
        }
    }
}

pub enum ConfigNetwork {
    Canary,
    Enjin,
}

impl From<ConfigNetwork> for crate::graphql::get_pending_transactions::Network {
    fn from(value: ConfigNetwork) -> Self {
        match value {
            ConfigNetwork::Canary => Self::CANARY,
            ConfigNetwork::Enjin => Self::ENJIN,
        }
    }
}

impl From<ConfigNetwork> for crate::graphql::get_pending_managed_wallet_creations::Network {
    fn from(value: ConfigNetwork) -> Self {
        match value {
            ConfigNetwork::Canary => Self::CANARY,
            ConfigNetwork::Enjin => Self::ENJIN,
        }
    }
}
