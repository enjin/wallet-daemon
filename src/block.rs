#![allow(dead_code)]
#![allow(unused)]

use std::sync::{Arc, Mutex};
use subxt::ext::subxt_core;
use subxt::{OnlineClient, PolkadotConfig};
use subxt_core::config::substrate;
use tokio::task::JoinHandle;

#[derive(Debug)]
pub struct BlockSubscription {
    rpc: Arc<OnlineClient<PolkadotConfig>>,
    block_header: Arc<Mutex<Option<substrate::SubstrateHeader<u32, substrate::BlakeTwo256>>>>,
}

impl BlockSubscription {
    pub fn new(rpc: Arc<OnlineClient<PolkadotConfig>>) -> Arc<Self> {
        let block_header = Arc::new(Mutex::new(None));

        let subscription = Arc::new(Self {
            rpc,
            block_header: Arc::clone(&block_header),
        });
        let subscription_clone = Arc::clone(&subscription);

        tokio::spawn(async move {
            subscription_clone.subscription().await.unwrap();
        });

        subscription
    }

    async fn subscription(self: Arc<Self>) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
        let mut blocks_sub = self.rpc.blocks().subscribe_finalized().await?;

        while let Some(block) = blocks_sub.next().await {
            let block = match block {
                Ok(b) => b,
                Err(e) => {
                    if e.is_disconnected_will_reconnect() {
                        tracing::warn!("Lost connection with the RPC node, reconnecting...");
                        continue;
                    }

                    return Err(e.into());
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

        Ok(())
    }

    pub fn get_block_header(&self) -> substrate::SubstrateHeader<u32, substrate::BlakeTwo256> {
        let block_header_lock = self.block_header.lock().unwrap();

        block_header_lock.clone().unwrap()
    }
}
