use reqwest::header::HeaderMap;
use std::sync::{Arc, OnceLock};
use subxt::Metadata;
use tokio::sync::RwLock;

// Mutable
pub static METADATA: RwLock<Option<Arc<Metadata>>> = RwLock::const_new(None);

// Immutable
pub(super) static HEADERS: OnceLock<HeaderMap> = OnceLock::new();
pub(super) static PLATFORM_URL: OnceLock<String> = OnceLock::new();

// getters
pub fn headers() -> HeaderMap {
    HEADERS.get().expect("headers not set").clone()
}

pub fn platform_url() -> &'static str {
    PLATFORM_URL.get().expect("platform url not set")
}
