#![allow(dead_code)]
#![allow(unused)]

use std::sync::{Arc, Mutex};
use subxt::{OnlineClient, PolkadotConfig};
use tokio::task::JoinHandle;

#[derive(Debug)]
pub struct BlockSubscription {
    rpc: OnlineClient<PolkadotConfig>,
    block_number: Arc<Mutex<u32>>,
}

impl BlockSubscription {
    pub fn new(rpc: OnlineClient<PolkadotConfig>) -> Arc<Self> {
        let block_number = Arc::new(Mutex::new(0));
        let subscription = Arc::new(Self {
            rpc,
            block_number: Arc::clone(&block_number),
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
                        tracing::warn!("Reconnecting to the RPC");
                        continue;
                    }

                    return Err(e.into());
                }
            };

            let block_number = block.number();
            let block_hash = block.hash();

            tracing::info!("Block #{block_number}: {block_hash}");

            let mut block_number_lock = self.block_number.lock().unwrap();
            *block_number_lock = block_number;
        }

        Ok(())
    }

    pub fn get_block_number(&self) -> u32 {
        let block_number_lock = self.block_number.lock().unwrap();
        *block_number_lock
    }
}
