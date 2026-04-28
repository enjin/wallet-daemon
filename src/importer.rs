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
