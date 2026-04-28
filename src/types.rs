use crate::SubstrateClient;
use clap::{Parser, Subcommand};
use std::sync::Arc;
use subxt::Metadata;

#[derive(Parser, Debug)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Import a wallet from a known seed
    Import,
    /// Prints the decrypted seed phrase. Appends password to v1 seed phrases.
    PrintSeed,
}

#[derive(Eq, Hash, PartialEq, Copy, Clone, Debug)]
pub enum Chain {
    Matrix,
    Relay,
}

impl From<Chain> for crate::graphql::get_pending_transactions::Chain {
    fn from(value: Chain) -> Self {
        match value {
            Chain::Matrix => Self::MATRIX,
            Chain::Relay => Self::RELAY,
        }
    }
}

impl TryFrom<crate::graphql::get_pending_transactions::Chain> for Chain {
    type Error = String;

    fn try_from(
        value: crate::graphql::get_pending_transactions::Chain,
    ) -> Result<Self, Self::Error> {
        match value {
            crate::graphql::get_pending_transactions::Chain::RELAY => Ok(Self::Relay),
            crate::graphql::get_pending_transactions::Chain::MATRIX => Ok(Self::Matrix),
            crate::graphql::get_pending_transactions::Chain::Other(e) => Err(e),
        }
    }
}

impl From<Chain> for crate::graphql::get_chain_info::Chain {
    fn from(value: Chain) -> Self {
        match value {
            Chain::Matrix => Self::MATRIX,
            Chain::Relay => Self::RELAY,
        }
    }
}

impl From<Chain> for crate::graphql::get_current_block_number::Chain {
    fn from(value: Chain) -> Self {
        match value {
            Chain::Matrix => Self::MATRIX,
            Chain::Relay => Self::RELAY,
        }
    }
}

impl From<Chain> for crate::graphql::get_account_nonce::Chain {
    fn from(value: Chain) -> Self {
        match value {
            Chain::Matrix => Self::MATRIX,
            Chain::Relay => Self::RELAY,
        }
    }
}

#[derive(Eq, Hash, PartialEq, Copy, Clone, Debug)]
pub enum Network {
    Canary,
    Enjin,
}

impl From<Network> for crate::graphql::get_pending_transactions::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Canary => Self::CANARY,
            Network::Enjin => Self::ENJIN,
        }
    }
}

impl TryFrom<crate::graphql::get_pending_transactions::Network> for Network {
    type Error = String;

    fn try_from(
        value: crate::graphql::get_pending_transactions::Network,
    ) -> Result<Self, Self::Error> {
        match value {
            crate::graphql::get_pending_transactions::Network::CANARY => Ok(Self::Canary),
            crate::graphql::get_pending_transactions::Network::ENJIN => Ok(Self::Enjin),
            crate::graphql::get_pending_transactions::Network::Other(e) => Err(e),
        }
    }
}

impl From<Network> for crate::graphql::get_chain_info::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Canary => Self::CANARY,
            Network::Enjin => Self::ENJIN,
        }
    }
}

impl From<Network> for crate::graphql::get_current_block_number::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Canary => Self::CANARY,
            Network::Enjin => Self::ENJIN,
        }
    }
}

impl From<Network> for crate::graphql::get_account_nonce::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Canary => Self::CANARY,
            Network::Enjin => Self::ENJIN,
        }
    }
}

pub struct MetadataInfo {
    pub spec_version: u32,
    pub metadata: Arc<Metadata>,
    pub client: SubstrateClient,
}
