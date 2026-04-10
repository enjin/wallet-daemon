use crate::crypto;
use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use std::path::Path;
use std::str::FromStr;
use std::{env, fs};
use subxt_signer::SecretUri;
use subxt_signer::bip39::Mnemonic;
use subxt_signer::sr25519::Keypair;

fn decrypt_mnemonic(content: &str) -> String {
    let stripped = content
        .trim()
        .trim_matches('"')
        .trim_matches('\n')
        .trim_matches('\r')
        .to_string();

    if crypto::is_encrypted(&stripped) {
        panic!("Encrypted seed files require KEY_PASS for decryption")
    }

    stripped
}

fn get_keys(seed_path: &Path, key_pass: &str) -> Keypair {
    let base_path = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = if seed_path.is_absolute() {
        seed_path.to_path_buf()
    } else {
        base_path.join(seed_path)
    };

    if path.is_file() {
        let content = fs::read_to_string(&path).expect("Unable to read file");

        let decrypted = if crypto::is_encrypted(&content) {
            crypto::decrypt(&content, key_pass).expect("Failed to decrypt seed")
        } else {
            decrypt_mnemonic(&content)
        };

        let secret = format!("{}///{}", decrypted, key_pass);
        let uri = SecretUri::from_str(&secret).expect("valid URI");
        return Keypair::from_uri(&uri).expect("valid keypair");
    }

    // TODO: this is currently dead code because path gets converted to a file before this function is called
    if let Ok(entries) = fs::read_dir(&path) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().unwrap().len() != 72 {
                continue;
            }

            let content = fs::read_to_string(entry.path()).expect("Unable to read file");

            let decrypted = if crypto::is_encrypted(&content) {
                crypto::decrypt(&content, key_pass).unwrap_or_else(|_| {
                    content
                        .strip_suffix("\r\n")
                        .or(content.strip_suffix("\n"))
                        .unwrap_or(&*content)
                        .replace("\"", "")
                })
            } else {
                decrypt_mnemonic(&content)
            };

            let secret = format!("{}///{}", decrypted, key_pass);

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
    let encrypted_mnemonic = crypto::encrypt(&mnemonic, key_pass);

    let secret = format!("{}///{}", mnemonic, key_pass);
    let uri = SecretUri::from_str(&secret).expect("valid URI");
    let keypair_tx = Keypair::from_uri(&uri).expect("valid keypair");

    fs::write(&path, encrypted_mnemonic).expect("Unable to write file");

    keypair_tx
}

pub fn load_wallet(seed_path: &Path, key_pass: &str) -> Keypair {
    let version = env!("CARGO_PKG_VERSION");
    let signer = get_keys(seed_path, key_pass);
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
