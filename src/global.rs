use std::sync::{Arc, OnceLock};
use reqwest::header::HeaderMap;
use subxt::Metadata;
use tokio::sync::RwLock;

// Mutable
pub static METADATA: RwLock<Option<Arc<Metadata>>> = RwLock::const_new(None);

// Immutable
pub static HEADERS: OnceLock<HeaderMap> = OnceLock::new();

// getters
pub fn headers() -> HeaderMap {
    HEADERS.get().expect("headers not set").clone()
}