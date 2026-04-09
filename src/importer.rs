use crate::crypto;
use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use std::fs;
use std::path::Path;
use std::str::FromStr;
use subxt_signer::sr25519::Keypair;
use subxt_signer::{ExposeSecret, SecretUri};

pub fn write_seed(seed: String, seed_path: &Path, key_pass: &str) -> std::io::Result<()> {
    let base_path = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = if seed_path.is_absolute() {
        seed_path.to_path_buf()
    } else {
        base_path.join(seed_path)
    };

    let uri = SecretUri::from_str(&seed).expect("valid URI");
    let keypair_tx = Keypair::from_uri(&uri).expect("valid keypair");

    let encrypted_mnemonic = crypto::encrypt(uri.phrase.expose_secret(), key_pass);

    let final_path = if path.is_dir() {
        path.join(format!(
            "73723235{}",
            hex::encode(keypair_tx.public_key().0)
        ))
    } else {
        path
    };

    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).ok();
    }

    fs::write(&final_path, encrypted_mnemonic).expect("Unable to write file");

    let public_key = keypair_tx.public_key().0;
    let account_id = sp_core::crypto::AccountId32::from(public_key);

    println!(
        "* Enjin Matrixchain  (SS58): {}",
        account_id.to_ss58check_with_version(Ss58AddressFormat::custom(1110))
    );
    println!(
        "* Canary Matrixchain (SS58): {}",
        account_id.to_ss58check_with_version(Ss58AddressFormat::custom(9030))
    );
    println!("* Public Key          (Hex): 0x{}", hex::encode(public_key));

    Ok(())
}
