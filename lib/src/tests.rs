use serial_test::serial;
use std::{env, fs::remove_dir_all, path::Path, sync::Arc, time::Duration};

use crate::{
    config_loader::{
        load_config, load_wallet_with_connections, Configuration, PairSig, CONFIG_FILE_ENV_NAME,
        KEY_PASS, PLATFORM_KEY,
    },
    connection::connection_handler::Connector,
    jobs::create_job_pair_with_executor,
    load_wallet,
    mock::{MockChainConnection, MockClient, MockEfinityDataProvider, MockEfinityWallet},
    wallet_trait::{KeyDerive, Wallet},
};
use codec::Encode;
use sp_core::Pair;
use sp_keyring::AccountKeyring;
use subxt::{DefaultConfig, Signer};

const EXTRINSIC: &'static str = "2800016400000000000000016400000000000000000000000000000000";
const SIGNED_EXTRINSIC: &'static str = "11028400d43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d0128b46d16901b3544e810c23632291ddd224afa4b9bc041df47a7375e1bf3610277eb58041d45fc3ceec2df308601b5139f4f615acf8acf0809d897ddfa695e8e030000002800016400000000000000016400000000000000000000000000000000";
const EXTRINSIC_STORE_SIGNATURE: &'static str = "0100b039724bca229094ce5758143d6801941f2430a993b9e758a9452f91797ebd0d014410bcbeed2e101442818626009f3d0898829ad2f7f3d640d06c5ddcfd9dea7e7f2f2fa28cc4680d8f9550b85c584d6210f8d10638dceff925fae14e3bae958d0300000000";
const DERIVED_SIGNED_EXTRINSIC: &'static str = "11028400b2955884765612b245243f634451ffd2e6f3453a70c8dad82d30c6600c0b6010014cb680629ffd3450ecaade6e3f0c17a2b8837966f4fff4c839b699f149d875226eef3bfd15667db83fc27a885af8a2a2795eefe0e7653bed1864daf31309e486030000002800016400000000000000016400000000000000000000000000000000";
const ALICE_ACCOUNT: &'static str = "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY";
const STORE_ACCOUNT: &'static str = "5G3mMFmXkP8gRt6RaSyXNGgLPF5DbYFr4JYcyiDKDCBc1wvq";

const MNEMONIC_PHRASE: &'static str =
    "total east north winner target fitness custom prize drive arrange snap dolphin";

const PAIR_PASSWORD: &'static str = "SuperStrongPassword995@";

#[tokio::test]
async fn wallet_signs() {
    let context_provider = Arc::new(MockEfinityDataProvider::new());
    let signer = PairSig::new(AccountKeyring::Alice.pair());
    let wallet = MockEfinityWallet::new(&context_provider, signer);
    let extrinsic = hex::decode(EXTRINSIC).unwrap();
    let actual = wallet.sign_transaction(extrinsic).await.unwrap();
    let expected = hex::decode(SIGNED_EXTRINSIC).unwrap();

    // The range [37..100] seems to be dependent on the time of the signature so we don't test for that.
    assert_eq!(actual.encode()[..37], expected[..37]);
    assert_eq!(actual.encode()[101..], expected[101..]);
}

#[tokio::test]
async fn wallet_correct_account_id() {
    let context_provider = Arc::new(MockEfinityDataProvider::new());
    let signer = PairSig::new(AccountKeyring::Alice.pair());
    let wallet = MockEfinityWallet::new(&context_provider, signer);

    assert_eq!(
        format!("{}", wallet.account_id().await.unwrap()),
        ALICE_ACCOUNT
    );
}

// Just testing it doesn't panic
#[tokio::test]
async fn wallet_reset_nonce() {
    let context_provider = Arc::new(MockEfinityDataProvider::new());
    let signer = PairSig::new(AccountKeyring::Alice.pair());
    let wallet = MockEfinityWallet::new(&context_provider, signer);
    wallet.reset_nonce().await.unwrap();
}

#[tokio::test]
async fn wallet_derivation() {
    let keypair = sp_core::sr25519::Pair::from_phrase(MNEMONIC_PHRASE, Some(PAIR_PASSWORD))
        .expect("Failed to create keypair");
    let context_provider = Arc::new(MockEfinityDataProvider::new());
    let signer = PairSig::new(keypair.0);

    assert_eq!(
        format!("{}", signer.account_id()),
        "5D27GnTkx4J8nreHAvW8fL1irobW7EPEpbhgbeRyVmVv8nsA"
    );

    let wallet = MockEfinityWallet::new(&context_provider, signer);
    let new_wallet = wallet.derive("player_1_id".into(), None).unwrap().0;
    assert_eq!(
        format!("{}", new_wallet.account_id().await.unwrap()),
        "5CS8AddHCnRmEFC9opAPqbrsTRtZ8CwAjKRhdnabCnc1tZZ6"
    );

    let new_wallet = wallet.derive("player_2_id".into(), None).unwrap().0;
    assert_eq!(
        format!("{}", new_wallet.account_id().await.unwrap()),
        "5CcrdaTD52u2E7Z4wmpwEmmnAdD5eAWRbi4M2ftShwbHQVFT"
    );

    let new_wallet = wallet.derive("player_9999_id".into(), None).unwrap().0;
    assert_eq!(
        format!("{}", new_wallet.account_id().await.unwrap()),
        "5G97pVHYwVBvcrpzLEhNWaQS8UFLHrxMqtYpE7wp5AQFBs94"
    );
}

#[tokio::test]
async fn wallet_derive_works() {
    let context_provider = Arc::new(MockEfinityDataProvider::new());
    let signer = PairSig::new(AccountKeyring::Alice.pair());
    let wallet = MockEfinityWallet::new(&context_provider, signer);
    let new_wallet = wallet.derive("player/1".into(), None).unwrap().0;
    // Gets new account id
    assert_eq!(
        format!("{}", new_wallet.account_id().await.unwrap()),
        "5G6rkCSunyiDGceZWSApdFGVxHtmRMoaFRdYXKj8WP2PmrXH"
    );

    // Can also correctly sign
    let extrinsic = hex::decode(EXTRINSIC).unwrap();
    let actual = new_wallet.sign_transaction(extrinsic).await.unwrap();
    let expected = hex::decode(DERIVED_SIGNED_EXTRINSIC).unwrap();

    // The range [37..100] seems to be dependent on the time of the signature, so we don't test for that.
    assert_eq!(actual.encode()[..37], expected[..37]);
    assert_eq!(actual.encode()[101..], expected[101..]);
}

const RELAY_NODE_URL: &'static str = "wss://rpc.relay.canary.enjin.io:443";
const NODE_URL: &'static str = "wss://rpc.matrix.canary.enjin.io:443";
const GRAPHQL_URL: &'static str = "https://platform.canary.enjin.io/graphql";
const PLATFORM_AUTH_KEY: &'static str = "KEY";
const KEY_STORAGE: &'static str = "tests/store";
const KEY_STORAGE_DEFAULT: &'static str = "store";
const KEY_PASSWORD: &'static str = "TEST";
const CONFIG_FILE: &'static str = "config-test.json";

fn set_config_path() {
    let config_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(CONFIG_FILE);
    env::set_var(CONFIG_FILE_ENV_NAME, config_path);
}

// NOTE!: Test that depends on the env variables are run serially to prevent errors
// from a test affecting another (Remember to clean up env variables if they are important to be unset)

#[tokio::test]
#[serial]
async fn load_config_works() {
    set_config_path();

    let config = load_config();
    assert_eq!(
        config,
        Configuration::new(
            NODE_URL.to_string(),
            RELAY_NODE_URL.to_string(),
            KEY_STORAGE.into(),
            GRAPHQL_URL.to_string()
        )
    );
}

#[tokio::test]
#[serial]
async fn load_config_default_works() {
    env::remove_var(CONFIG_FILE_ENV_NAME);
    let config = load_config();
    assert_eq!(
        config,
        Configuration::new(
            NODE_URL.to_string(),
            RELAY_NODE_URL.to_string(),
            KEY_STORAGE_DEFAULT.into(),
            GRAPHQL_URL.to_string()
        )
    );
}

#[tokio::test]
#[serial]
async fn load_wallet_works() {
    set_config_path();
    env::set_var(KEY_PASS, KEY_PASSWORD);
    env::set_var(PLATFORM_KEY, PLATFORM_AUTH_KEY);
    let config = load_config();
    let (wallet_connection_pair, graphql_url, token) = load_wallet::<DefaultConfig>(config).await;
    assert_eq!(graphql_url, GRAPHQL_URL);
    assert_eq!(token, PLATFORM_AUTH_KEY);
    assert_eq!(
        format!(
            "{}",
            wallet_connection_pair.wallet.account_id().await.unwrap()
        ),
        STORE_ACCOUNT
    );
}

#[tokio::test]
#[serial]
async fn load_wallet_works_new_key() {
    env::remove_var(CONFIG_FILE_ENV_NAME);
    env::set_var(KEY_PASS, KEY_PASSWORD);
    env::set_var(PLATFORM_KEY, PLATFORM_AUTH_KEY);
    let config = load_config();
    let (wallet_connection_pair, graphql_url, token) = load_wallet::<DefaultConfig>(config).await;
    assert_eq!(graphql_url, GRAPHQL_URL);
    assert_eq!(token, PLATFORM_AUTH_KEY);
    assert_ne!(
        format!(
            "{}",
            wallet_connection_pair.wallet.account_id().await.unwrap()
        ),
        STORE_ACCOUNT
    );

    // Clean up new storage
    remove_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/store")).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn jobs_work() {
    set_config_path();
    env::set_var(KEY_PASS, KEY_PASSWORD);
    env::set_var(PLATFORM_KEY, PLATFORM_AUTH_KEY);
    let config = load_config();
    let context_provider = Arc::new(MockEfinityDataProvider::<DefaultConfig>::new());
    let chain_connection = Arc::new(MockChainConnection::new());
    let (wallet_connection_pair, graphql_url, token) =
        load_wallet_with_connections::<DefaultConfig, _, _>(
            config,
            context_provider,
            chain_connection.clone(),
        )
        .await;
    let client = Arc::new(reqwest::Client::new());
    let client_executor = Arc::new(MockClient::new());
    let (poll_job, sign_processor) = create_job_pair_with_executor(
        graphql_url,
        token,
        String::from("matrix"),
        Duration::from_millis(6000),
        Arc::new(wallet_connection_pair),
        1000,
        client,
        client_executor.clone(),
    );
    poll_job.start_job();
    sign_processor.start_job();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    assert_eq!(chain_connection.get_transactions().len(), 1);
    let transaction_signed = chain_connection.get_transactions().first().unwrap().clone();
    let expected_sign = hex::decode(EXTRINSIC_STORE_SIGNATURE).unwrap();
    assert_eq!(
        transaction_signed.function.encode(),
        hex::decode(EXTRINSIC).unwrap()
    );
    assert_eq!(
        transaction_signed.signature.encode()[..35],
        expected_sign[..35]
    );

    assert_eq!(
        transaction_signed.signature.encode()[99..],
        expected_sign[99..]
    );
    assert_eq!(client_executor.get_hashes().len(), 1);
    let hash = client_executor.get_hashes().first().unwrap().clone();
    assert_eq!(
        hash,
        "\"0x0000000000000000000000000000000000000000000000000000000000000000\"".to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn jobs_work_twice() {
    set_config_path();
    env::set_var(KEY_PASS, KEY_PASSWORD);
    env::set_var(PLATFORM_KEY, PLATFORM_AUTH_KEY);
    let config = load_config();
    let context_provider = Arc::new(MockEfinityDataProvider::<DefaultConfig>::new());
    let chain_connection = Arc::new(MockChainConnection::new());
    let (wallet_connection_pair, graphql_url, token) =
        load_wallet_with_connections::<DefaultConfig, _, _>(
            config,
            context_provider,
            chain_connection.clone(),
        )
        .await;
    let client = Arc::new(reqwest::Client::new());
    let client_executor = Arc::new(MockClient::new());
    let (poll_job, sign_processor) = create_job_pair_with_executor(
        graphql_url,
        token,
        String::from("matrix"),
        Duration::from_millis(10),
        Arc::new(wallet_connection_pair),
        1000,
        client,
        client_executor.clone(),
    );
    poll_job.start_job();
    sign_processor.start_job();
    tokio::time::sleep(tokio::time::Duration::from_millis(15)).await;
    assert_eq!(chain_connection.get_transactions().len(), 2);
    assert_eq!(client_executor.get_hashes().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn jobs_work_trice() {
    set_config_path();
    env::set_var(KEY_PASS, KEY_PASSWORD);
    env::set_var(PLATFORM_KEY, PLATFORM_AUTH_KEY);
    let config = load_config();
    let context_provider = Arc::new(MockEfinityDataProvider::<DefaultConfig>::new());
    let chain_connection = Arc::new(MockChainConnection::new());
    let (wallet_connection_pair, graphql_url, token) =
        load_wallet_with_connections::<DefaultConfig, _, _>(
            config,
            context_provider,
            chain_connection.clone(),
        )
        .await;
    let client = Arc::new(reqwest::Client::new());
    let client_executor = Arc::new(MockClient::new());
    let (poll_job, sign_processor) = create_job_pair_with_executor(
        graphql_url,
        token,
        String::from("matrix"),
        Duration::from_millis(10),
        Arc::new(wallet_connection_pair),
        1000,
        client,
        client_executor.clone(),
    );
    poll_job.start_job();
    sign_processor.start_job();
    tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
    assert_eq!(chain_connection.get_transactions().len(), 3);
    assert_eq!(client_executor.get_hashes().len(), 3);
}

#[tokio::test]
#[serial]
async fn connector_works() {
    set_config_path();
    env::set_var(KEY_PASS, KEY_PASSWORD);
    env::set_var(PLATFORM_KEY, PLATFORM_AUTH_KEY);
    let config = load_config();
    let context_provider = Arc::new(Connector::<
        MockEfinityDataProvider<DefaultConfig>,
        String,
        subxt::BasicError,
    >::new("test".to_string()));
    let chain_connection = Arc::new(
        Connector::<MockChainConnection, String, subxt::BasicError>::new("test".to_string()),
    );
    let (wallet_connection_pair, graphql_url, token) =
        load_wallet_with_connections::<DefaultConfig, _, _>(
            config,
            context_provider,
            chain_connection.clone(),
        )
        .await;
    let client = Arc::new(reqwest::Client::new());
    let client_executor = Arc::new(MockClient::new());
    let (poll_job, sign_processor) = create_job_pair_with_executor(
        graphql_url,
        token,
        String::from("matrix"),
        Duration::from_millis(10),
        Arc::new(wallet_connection_pair),
        1000,
        client,
        client_executor.clone(),
    );
    poll_job.start_job();
    sign_processor.start_job();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    assert_eq!(chain_connection.get_transactions().await.unwrap().len(), 1);
    let transaction_signed = chain_connection
        .get_transactions()
        .await
        .unwrap()
        .first()
        .unwrap()
        .clone();
    let expected_sign = hex::decode(EXTRINSIC_STORE_SIGNATURE).unwrap();

    assert_eq!(
        transaction_signed.function.encode(),
        hex::decode(EXTRINSIC).unwrap()
    );
    assert_eq!(
        transaction_signed.signature.encode()[..35],
        expected_sign[..35]
    );

    assert_eq!(
        transaction_signed.signature.encode()[99..],
        expected_sign[99..]
    );
    assert_eq!(client_executor.get_hashes().len(), 1);
    let hash = client_executor.get_hashes().first().unwrap().clone();
    assert_eq!(
        hash,
        "\"0x0000000000000000000000000000000000000000000000000000000000000000\"".to_string()
    );
}
