use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::{fmt, panic};
use subxt::client::ClientRuntimeUpdater;
use subxt::dynamic::At;
use subxt::{OnlineClient, PolkadotConfig};
use tokio::task::JoinHandle;

#[derive(Debug)]
pub struct SubscriptionJob {
    params: Arc<SubscriptionParams>,
}

#[derive(Debug, Clone)]
pub enum Network {
    EnjinRelay,
    CanaryRelay,
    EnjinMatrix,
    CanaryMatrix,
}

impl Network {
    pub fn to_query_var(&self) -> Option<String> {
        match self {
            Network::EnjinRelay => Some("relay".to_string()),
            Network::CanaryRelay => Some("relay".to_string()),
            _ => Some("matrix".to_string()),
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Network::EnjinRelay => write!(f, "Enjin Relaychain"),
            Network::CanaryRelay => write!(f, "Canary Relaychain"),
            Network::EnjinMatrix => write!(f, "Enjin Matrixchain"),
            Network::CanaryMatrix => write!(f, "Canary Matrixchain"),
        }
    }
}

impl FromStr for Network {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "enjin" => Ok(Network::EnjinRelay),
            "matrix-enjin" => Ok(Network::EnjinMatrix),
            "canary" => Ok(Network::CanaryRelay),
            "matrix" => Ok(Network::CanaryMatrix),
            _ => Err(format!("Unknown network: {}", s)),
        }
    }
}

impl SubscriptionJob {
    pub fn new(params: Arc<SubscriptionParams>) -> Self {
        Self { params }
    }

    pub fn create_job(rpc: Arc<OnlineClient<PolkadotConfig>>) -> SubscriptionJob {
        let spec_version = Arc::new(Mutex::new(None));

        let system_version = rpc
            .constants()
            .at(&subxt::dynamic::constant("System", "Version"))
            .unwrap();
        let system_version = system_version.to_value().unwrap();
        let spec_name_value = system_version.at("spec_name").unwrap();

        // Handle both cases: direct Primitive or Composite(Unnamed([...]))
        let spec_name_str = spec_name_value
            .as_str()
            .or_else(|| spec_name_value.at(0).and_then(|v| v.as_str()))
            .unwrap();

        let network = Network::from_str(spec_name_str).unwrap();

        let subscription = Arc::new(SubscriptionParams {
            rpc,
            spec_version: Arc::clone(&spec_version),
            network: Arc::clone(&Arc::new(network)),
        });

        SubscriptionJob::new(subscription)
    }

    pub fn start(&self) -> JoinHandle<()> {
        let runtime_sub = Arc::clone(&self.params);
        let updater = runtime_sub.rpc.updater();

        tokio::spawn(async move {
            tokio::select! {
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
    spec_version: Arc<Mutex<Option<u32>>>,
    network: Arc<Network>,
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
                    tracing::info!(
                        "{} has been upgraded to specVersion {} successfully",
                        self.network,
                        version
                    )
                }
                Err(e) => match *spec_version {
                    Some(v) => {
                        if v == version {
                            continue;
                        }

                        tracing::error!(
                            "{} has failed to upgrade to specVersion {} with error {:?}. Please restart your daemon.",
                            self.network,
                            version,
                            e
                        );
                    }
                    None => {
                        *spec_version = Some(version);
                        tracing::info!(
                            "Using specVersion {} to sign transactions for {}",
                            version,
                            self.network
                        );
                    }
                },
            };
        }

        tracing::error!("Runtime update stream ended unexpectedly");
    }

    pub fn get_network(&self) -> Arc<Network> {
        Arc::clone(&self.network)
    }
}
