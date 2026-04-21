use crate::SubstrateClient;
use crate::types::{Chain, MetadataInfo, Network};
use reqwest::header::HeaderMap;
use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};
use subxt::ext::frame_decode::extrinsics::ExtrinsicTypeInfo;
use tokio::sync::RwLock;

// Mutable
/// Stores metadata, spec_version, and client
static METADATA: LazyLock<RwLock<HashMap<(Network, Chain), MetadataInfo>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
/// The graphql client that fetches chain info
static GRAPHQL_CLIENT: LazyLock<RwLock<reqwest::Client>> =
    LazyLock::new(|| RwLock::new(reqwest::Client::new()));

// Immutable
pub(super) static HEADERS: OnceLock<HeaderMap> = OnceLock::new();
pub(super) static PLATFORM_URL: OnceLock<String> = OnceLock::new();

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
    METADATA
        .read()
        .await
        .get(&(network, chain))?
        .metadata
        .extrinsic_call_info_by_index(pallet_index, call_index)
        .ok()
        .map(|x| (x.pallet_name.to_string(), x.call_name.to_string()))
}

pub async fn substrate_client(network: Network, chain: Chain) -> Option<SubstrateClient> {
    METADATA
        .read()
        .await
        .get(&(network, chain))
        .map(|m| m.client.clone())
}

/// Get a reference to the client. The internal type is Arc, so cloning gives a reference.
pub async fn graphql_client() -> reqwest::Client {
    GRAPHQL_CLIENT.read().await.clone()
}
