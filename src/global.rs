use crate::types::{Chain, MetadataInfo, Network};
use reqwest::header::HeaderMap;
use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};
use subxt::ext::frame_decode::extrinsics::ExtrinsicTypeInfo;
use tokio::sync::RwLock;

// Mutable
static METADATA: LazyLock<RwLock<HashMap<(Network, Chain), MetadataInfo>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
pub static GRAPHQL_CLIENT: LazyLock<RwLock<reqwest::Client>> =
    LazyLock::new(|| RwLock::new(reqwest::Client::new()));
/// TODO: can make an enum for the configs and implement Config
// static SUBSTRATE_CLIENTS: LazyLock<RwLock<HashMap<(Network, Chain), OfflineClient<>>>> =
//     LazyLock::new(|| RwLock::new(HashMap::new()));

// Immutable
pub(super) static HEADERS: OnceLock<HeaderMap> = OnceLock::new();
pub(super) static PLATFORM_URL: OnceLock<String> = OnceLock::new();
/// The qraphql client that fetches chain info

// setters
pub async fn insert_metadata(network: Network, chain: Chain, metadata: MetadataInfo) {
    METADATA.write().await.insert((network, chain), metadata);
}

// getters
pub fn headers() -> HeaderMap {
    HEADERS.get().expect("headers not set").clone()
}

pub fn platform_url() -> &'static str {
    PLATFORM_URL.get().expect("platform url not set")
}

pub async fn metadata_spec_version(network: Network, chain: Chain) -> Option<u32> {
    METADATA
        .read()
        .await
        .get(&(network, chain))
        .map(|x| x.spec_version)
}

pub async fn metadata_names(
    network: Network,
    chain: Chain,
    pallet_index: u8,
    call_index: u8,
) -> Option<(String, String)> {
    // TODO: avoid cloning the strings here
    METADATA
        .read()
        .await
        .get(&(network, chain))?
        .metadata
        .extrinsic_call_info_by_index(pallet_index, call_index)
        .ok()
        .map(|x| (x.pallet_name.to_string(), x.call_name.to_string()))
}
