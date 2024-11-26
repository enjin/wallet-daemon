use std::sync::{Arc, Mutex};
use std::{panic};
use subxt::client::ClientRuntimeUpdater;
use subxt::ext::subxt_core;
use subxt::{OnlineClient, PolkadotConfig};
use subxt_core::config::substrate;
use tokio::task::JoinHandle;

#[derive(Debug)]
pub struct SubscriptionJob {
    params: Arc<SubscriptionParams>,
}

impl SubscriptionJob {
    pub fn new(params: Arc<SubscriptionParams>) -> Self {
        Self { params }
    }

    pub fn create_job(rpc: Arc<OnlineClient<PolkadotConfig>>) -> SubscriptionJob {
        let block_header = Arc::new(Mutex::new(None));
        let spec_version = Arc::new(Mutex::new(None));
        let subscription = Arc::new(SubscriptionParams {
            rpc,
            block_header: Arc::clone(&block_header),
            spec_version: Arc::clone(&spec_version),
        });

        SubscriptionJob::new(subscription)
    }

    pub fn start(&self) -> JoinHandle<()> {
        let block_sub = Arc::clone(&self.params);
        let runtime_sub = Arc::clone(&self.params);
        let updater = runtime_sub.rpc.updater();

        tokio::spawn(async move {
            tokio::select! {
                _ = block_sub.block_subscription() => {}
                _ = runtime_sub.runtime_subscription(updater) => {}
            }
        })
    }

    pub fn get_params(&self) -> Arc<SubscriptionParams> {
        Arc::clone(&self.params)
    }
}

#[derive(Debug)]
pub struct SubscriptionParams {
    rpc: Arc<OnlineClient<PolkadotConfig>>,
    block_header: Arc<Mutex<Option<substrate::SubstrateHeader<u32, substrate::BlakeTwo256>>>>,
    spec_version: Arc<Mutex<Option<u32>>>,
}

impl SubscriptionParams {
    async fn runtime_subscription(self: Arc<Self>, updater: ClientRuntimeUpdater<PolkadotConfig>) {
        let mut update_stream = updater.runtime_updates().await.unwrap();

        while let Some(Ok(update)) = update_stream.next().await {
            let version = update.runtime_version().spec_version;
            let mut spec_version = self.spec_version.lock().unwrap();

            match updater.apply_update(update) {
                Ok(()) => {
                    *spec_version = Some(version);
                    tracing::info!("Upgrade to specVersion: {} successful", version)
                }
                Err(e) => match *spec_version {
                    Some(v) => {
                        if v == version {
                            continue;
                        }

                        tracing::error!(
                            "Upgrade to specVersion {} failed {:?}. Please restart your daemon.",
                            version,
                            e
                        );
                    }
                    None => {
                        *spec_version = Some(version);
                        tracing::info!("Using specVersion {} to sign transactions", version);
                    }
                },
            };
        }

        tracing::error!("Runtime update stream ended unexpectedly");
    }

    async fn block_subscription(self: Arc<Self>) {
        let mut blocks_sub = self.rpc.blocks().subscribe_finalized().await.unwrap();

        while let Some(block) = blocks_sub.next().await {
            let block = match block {
                Ok(b) => b,
                Err(e) => {
                    if e.is_disconnected_will_reconnect() {
                        tracing::warn!("Lost connection with the RPC node, reconnecting...");
                    }

                    continue;
                }
            };

            tracing::info!(
                "Current finalized block #{}: {}",
                block.number(),
                block.hash()
            );

            let mut block_header = self.block_header.lock().unwrap();

            *block_header = Some(block.header().clone());
        }

        tracing::error!("Block subscription stream ended unexpectedly");
    }

    pub fn get_block_header(&self) -> substrate::SubstrateHeader<u32, substrate::BlakeTwo256> {
        let block_header_lock = self.block_header.lock().unwrap();

        block_header_lock.clone().unwrap()
    }
}
