use std::sync::Arc;
use subxt::Metadata;
use tokio::sync::RwLock;

pub(crate) static METADATA: RwLock<Option<Arc<Metadata>>> = RwLock::const_new(None);
