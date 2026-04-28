use crate::crypto;
use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use std::path::{Path, PathBuf};
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

    if crypto::is_encrypted_v2(&stripped) {
        panic!("Encrypted seed files require KEY_PASS for decryption")
    }

    stripped
}

pub fn load_seed(seed_path: &str, key_pass: &str, print_seed: bool) -> Keypair {
    let seed_path = PathBuf::from_str(seed_path).expect("SEED_PATH must be a valid path");
    if !seed_path.exists() {
        panic!("SEED_PATH does not exist: {:?}", seed_path)
    };

    let mut v1_migration_path: Option<PathBuf> = None;
    let seed_dir = if seed_path.is_dir() {
        seed_path
    } else {
        seed_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or(seed_path.clone())
    };
    let mut seed_path = if seed_dir.join("wallet.seed").exists() {
        seed_dir.join("wallet.seed")
    } else {
        let mut backup_seed_path = None;
        if let Ok(entries) = fs::read_dir(&seed_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && name.starts_with("73723235")
                    && name.len() == 72
                {
                    backup_seed_path = Some(entry.path());
                    break;
                }
            }
        }
        if backup_seed_path.is_some() {
            v1_migration_path = backup_seed_path.clone();
        }
        backup_seed_path.unwrap_or(seed_dir.join("wallet.seed"))
    };
    tracing::debug!("loading seed from path: {}", seed_path.display());

    if let Some(v1_path) = v1_migration_path {
        tracing::debug!("Found v1 seed file, migrating to v2 format...");
        let v1_content = fs::read_to_string(&v1_path).expect("Unable to read v1 file");
        let mnemonic = decrypt_mnemonic(&v1_content);
        let v2_encrypted = crypto::encrypt_with_imported(&mnemonic, key_pass, true);
        let wallet_seed_path = seed_dir.join("wallet.seed");
        fs::write(&wallet_seed_path, &v2_encrypted).expect("Unable to write wallet.seed");

        let verified_keypair = load_wallet(&wallet_seed_path, key_pass, PrintKind::Nothing);

        let v1_keypair = load_wallet(&v1_path, key_pass, PrintKind::Nothing);

        if verified_keypair.public_key().0 != v1_keypair.public_key().0 {
            panic!("V2 migration failed: public key mismatch");
        }
        tracing::info!("Successfully migrated seed to new format");
        fs::remove_file(&v1_path).expect("Unable to delete v1 file");
        seed_path = wallet_seed_path;
    }

    let print_kind = if print_seed {
        PrintKind::Seed
    } else {
        PrintKind::Normal
    };
    load_wallet(&seed_path, key_pass, print_kind)
}

fn get_keys(seed_path: &Path, key_pass: &str, print_seed: bool) -> Keypair {
    let base_path = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = if seed_path.is_absolute() {
        seed_path.to_path_buf()
    } else {
        base_path.join(seed_path)
    };

    if path.is_file() {
        let content = fs::read_to_string(&path).expect("Unable to read file");

        let decrypted = match crypto::decrypt(&content, key_pass) {
            Ok(decrypted) => decrypted,
            Err(_) => {
                let mnemonic = decrypt_mnemonic(&content);
                format!("{}///{}", mnemonic, key_pass)
            }
        };

        let uri = SecretUri::from_str(&decrypted).expect("valid URI");
        if print_seed {
            println!("{}", &decrypted);
        }
        return Keypair::from_uri(&uri).expect("valid keypair");
    }

    let (mnemonic, keypair) = write_mnemonic(&path, key_pass, None);
    if print_seed {
        println!("{}", &mnemonic);
    }
    keypair
}

pub fn write_mnemonic(
    seed_path: &Path,
    key_pass: &str,
    mnemonic: Option<&str>,
) -> (String, Keypair) {
    let base_path = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = if seed_path.is_absolute() {
        seed_path.to_path_buf()
    } else {
        base_path.join(seed_path)
    };

    let mnemonic = match mnemonic {
        Some(m) => m.to_string(),
        None => Mnemonic::generate(12).unwrap().to_string(),
    };

    let encrypted_mnemonic = crypto::encrypt(&mnemonic, key_pass);
    let uri = SecretUri::from_str(&mnemonic).expect("valid URI");
    let keypair_tx = Keypair::from_uri(&uri).expect("valid keypair");

    let final_path = if path.is_dir() {
        path.join("wallet.seed")
    } else {
        path
    };

    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).ok();
    }

    fs::write(&final_path, encrypted_mnemonic).expect("Unable to write file");

    (mnemonic, keypair_tx)
}

#[derive(PartialEq, Eq, Clone, Copy)]
/// What prints in `load_wallet`
pub enum PrintKind {
    /// Print normal output
    Normal,
    /// Print nothing
    Nothing,
    /// Print the private key and mnemonic
    Seed,
}

pub fn load_wallet(seed_path: &Path, key_pass: &str, print_kind: PrintKind) -> Keypair {
    let version = env!("CARGO_PKG_VERSION");
    let signer = get_keys(seed_path, key_pass, print_kind == PrintKind::Seed);
    let public_key = signer.public_key().0;
    let account_id = sp_core::crypto::AccountId32::from(public_key);

    if print_kind == PrintKind::Normal {
        println!("******************* Enjin Wallet Daemon v{version} *******************");
        println!(
            "** Enjin Relaychain   (SS58): {}",
            account_id.to_ss58check_with_version(Ss58AddressFormat::custom(2135))
        );
        println!(
            "** Enjin Matrixchain  (SS58): {}",
            account_id.to_ss58check_with_version(Ss58AddressFormat::custom(1110))
        );
        println!(
            "** Canary Relaychain  (SS58): {}",
            account_id.to_ss58check_with_version(Ss58AddressFormat::custom(69))
        );
        println!(
            "** Canary Matrixchain (SS58): {}",
            account_id.to_ss58check_with_version(Ss58AddressFormat::custom(9030))
        );
        println!(
            "** Public Key          (Hex): 0x{}",
            hex::encode(public_key)
        );
    }

    signer
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_v1_to_v2_migration() {
        let temp_dir = TempDir::new().unwrap();
        let key_pass = "test_password";

        let pubkey_suffix = "a82a0376985e4bdca417ceebc52499664ac78437e5ae074de72907a1b42b643e";
        let v1_file_name = format!("73723235{}", pubkey_suffix);
        let v1_path = temp_dir.path().join(&v1_file_name);
        let mut v1_file = fs::File::create(&v1_path).unwrap();
        write!(
            v1_file,
            "\"muscle wing great bounce arctic guess trim celery budget shock march whale\""
        )
        .unwrap();
        drop(v1_file);

        let seed_path = temp_dir.path().to_str().unwrap();
        let keypair = load_seed(seed_path, key_pass, false);
        let migrated_public_key = keypair.public_key().0;

        let wallet_seed_path = temp_dir.path().join("wallet.seed");
        assert!(
            wallet_seed_path.exists(),
            "wallet.seed should exist after migration"
        );
        assert!(
            !v1_path.exists(),
            "v1 file should be deleted after migration"
        );

        let keypair2 = load_seed(seed_path, key_pass, false);
        assert_eq!(
            migrated_public_key,
            keypair2.public_key().0,
            "public key should match after migration"
        );
    }

    #[test]
    fn test_load_existing_wallet_seed() {
        let temp_dir = TempDir::new().unwrap();
        let key_pass = "test_password";
        let mnemonic = "muscle wing great bounce arctic guess trim celery budget shock march whale";
        let encrypted = crypto::encrypt_with_imported(mnemonic, key_pass, true);

        let wallet_seed_path = temp_dir.path().join("wallet.seed");
        fs::write(&wallet_seed_path, &encrypted).unwrap();

        let seed_path = temp_dir.path().to_str().unwrap();
        let keypair = load_seed(seed_path, key_pass, false);

        let v1_path = temp_dir.path().join("73723235test");
        let mut v1_file = fs::File::create(&v1_path).unwrap();
        write!(v1_file, "\"{}\"", mnemonic).unwrap();
        drop(v1_file);

        let keypair_v1 = load_wallet(&v1_path, key_pass, PrintKind::Nothing);
        assert_eq!(
            keypair.public_key().0,
            keypair_v1.public_key().0,
            "wallet.seed should load to same key as v1 file"
        );
    }
}
