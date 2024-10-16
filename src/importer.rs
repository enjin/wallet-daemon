use secrecy::ExposeSecret;
use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use std::fs;
use std::path::Path;
use std::str::FromStr;
use subxt_signer::sr25519::Keypair;
use subxt_signer::SecretUri;

pub fn write_seed(seed: String) -> std::io::Result<()> {
    // TODO: Get the path for the store from config_loader
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("store");
    let uri = SecretUri::from_str(&*seed).expect("valid URI");
    let keypair_tx = Keypair::from_uri(&uri).expect("valid keypair");

    fs::write(
        p.join(format!(
            "73723235{}",
            hex::encode(keypair_tx.public_key().0)
        )),
        format!("\"{}\"", uri.phrase.expose_secret()),
    )
    .expect("Unable to write file");

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
