//! Fixtures shared by unit tests in several modules.

use std::sync::Arc;
use subxt::Metadata;

/// Load a metadata fixture from `tests/fixtures`.
pub fn load_metadata_from(filename: &str) -> Arc<Metadata> {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), filename);
    let bytes = std::fs::read(&path).expect("metadata fixture missing");
    Arc::new(Metadata::decode_from(&bytes).expect("decode metadata"))
}
