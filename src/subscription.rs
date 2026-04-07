use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::{fmt, panic};
use subxt::client::ClientRuntimeUpdater;
use subxt::dynamic::At;
use subxt::{OfflineClient, PolkadotConfig};
use tokio::task::JoinHandle;

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
