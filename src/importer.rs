use crate::wallet_loader::write_mnemonic;
use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use std::path::Path;

pub fn write_seed(seed: String, seed_path: &Path, key_pass: &str) -> std::io::Result<()> {
    let (_, keypair_tx) = write_mnemonic(seed_path, key_pass, Some(&seed));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_seed;
    use tempfile::TempDir;

    #[test]
    fn test_write_seed_creates_valid_wallet() {
        let temp_dir = TempDir::new().unwrap();
        let key_pass = "test_password";
        let mnemonic = "muscle wing great bounce arctic guess trim celery budget shock march whale";

        write_seed(mnemonic.to_string(), temp_dir.path(), key_pass).unwrap();

        let wallet_seed_path = temp_dir.path().join("wallet.seed");
        assert!(wallet_seed_path.exists(), "wallet.seed should be created");

        let content = std::fs::read_to_string(&wallet_seed_path).unwrap();
        assert!(
            crate::crypto::is_encrypted_v2(&content),
            "should be v2 encrypted"
        );
    }

    #[test]
    fn test_write_seed_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let key_pass = "test_password";
        let mnemonic = "muscle wing great bounce arctic guess trim celery budget shock march whale";

        write_seed(mnemonic.to_string(), temp_dir.path(), key_pass).unwrap();

        let seed_path = temp_dir.path().to_str().unwrap();
        let loaded_keypair = load_seed(seed_path, key_pass, true);
        let reloaded_keypair = load_seed(seed_path, key_pass, false);

        assert_eq!(
            loaded_keypair.public_key().0,
            reloaded_keypair.public_key().0,
            "loaded keypair should match reloaded"
        );
    }

    #[test]
    fn test_write_seed_wrong_password_fails() {
        let temp_dir = TempDir::new().unwrap();
        let key_pass = "test_password";
        let wrong_pass = "wrong_password";
        let mnemonic = "muscle wing great bounce arctic guess trim celery budget shock march whale";

        write_seed(mnemonic.to_string(), temp_dir.path(), key_pass).unwrap();

        let seed_path = temp_dir.path().to_str().unwrap();
        let result = std::panic::catch_unwind(|| load_seed(seed_path, wrong_pass, false));
        assert!(result.is_err(), "wrong password should panic");
    }

    #[test]
    fn test_write_seed_produces_correct_addresses() {
        let temp_dir = TempDir::new().unwrap();
        let key_pass = "test_password";
        let mnemonic = "muscle wing great bounce arctic guess trim celery budget shock march whale";

        write_seed(mnemonic.to_string(), temp_dir.path(), key_pass).unwrap();

        let keypair = load_seed(temp_dir.path().to_str().unwrap(), key_pass, false);
        let public_key = keypair.public_key().0;
        let account_id = sp_core::crypto::AccountId32::from(public_key);

        let enjin_matrixchain =
            account_id.to_ss58check_with_version(Ss58AddressFormat::custom(1110));
        let canary_matrixchain =
            account_id.to_ss58check_with_version(Ss58AddressFormat::custom(9030));

        assert!(
            enjin_matrixchain.starts_with("ef"),
            "Enjin matrixchain should start with ef"
        );
        assert!(
            canary_matrixchain.starts_with("cx"),
            "Canary matrixchain should start with cx"
        );
    }
}
