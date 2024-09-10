#![allow(dead_code)]
#![allow(unused)]

use config::Config;
use secrecy::ExposeSecret;
use serde::Deserialize;
use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{env, fs};
use subxt_signer::bip39::Mnemonic;
use subxt_signer::sr25519::Keypair;
use subxt_signer::{SecretString, SecretUri};

pub(crate) const CONFIG_FILE_ENV_NAME: &str = "CONFIG_FILE";
pub(crate) const DEFAULT_CONFIG_FILE: &str = "config.json";
pub(crate) const KEY_PASS: &str = "KEY_PASS";
pub(crate) const PLATFORM_KEY: &str = "PLATFORM_KEY";

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Configuration {
    node: String,
    relay_node: String,
    master_key: PathBuf,
    api: String,
}

impl Configuration {
    pub(crate) fn new(node: String, relay_node: String, master_key: PathBuf, api: String) -> Self {
        Self {
            node,
            relay_node,
            master_key,
            api,
        }
    }
}

pub fn load_config() -> Configuration {
    let config_file = env::var(CONFIG_FILE_ENV_NAME).unwrap_or_else(|_| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(DEFAULT_CONFIG_FILE)
            .to_str()
            .unwrap()
            .to_string()
    });

    Config::builder()
        .add_source(config::File::from(Path::new(&config_file)))
        .build()
        .expect("Configuration file not found")
        .try_deserialize::<Configuration>()
        .expect("Configuration is incorrect")
}

fn get_password(env_name: &str) -> SecretString {
    SecretString::new(
        env::var(env_name).expect(&format!("Password {} not loaded in memory", env_name)),
    )
}

async fn get_keys(key_store_path: &Path, password: SecretString) -> Keypair {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(key_store_path);

    if let Ok(entries) = fs::read_dir(&p) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().unwrap().len() != 72 {
                continue;
            }

            let content = fs::read_to_string(entry.path()).expect("Unable to read file");
            let strip_content = content.strip_suffix("\r\n").or(content.strip_suffix("\n")).unwrap_or(&*content).to_string();
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

pub async fn load_wallet(config: Configuration) -> (Keypair, String, String, String, String) {
    let password = get_password(KEY_PASS);
    let signer = get_keys(&config.master_key, password).await;
    let public_key = signer.public_key().0;
    let account_id = sp_core::crypto::AccountId32::from(public_key);

    println!("********** Enjin Wallet Daemon v2.0.0 - Loaded Wallet **********");
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
    println!("** Matrixchain RPC          : {}", config.node);
    println!("** Relaychain RPC           : {}", config.relay_node);
    println!("** Platform URL             : {}", config.api);
    println!("*****************************************************************");

    (
        signer,
        config.node,
        config.relay_node,
        config.api,
        env::var(PLATFORM_KEY).unwrap_or_default(),
    )
}
