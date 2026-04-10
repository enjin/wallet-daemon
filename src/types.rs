#[derive(Eq, Hash, PartialEq)]
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

impl From<Chain> for crate::graphql::get_pending_managed_wallet_creations::Chain {
    fn from(value: Chain) -> Self {
        match value {
            Chain::Matrix => Self::MATRIX,
            Chain::Relay => Self::RELAY,
        }
    }
}

#[derive(Eq, Hash, PartialEq)]
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

impl From<Network> for crate::graphql::get_pending_managed_wallet_creations::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Canary => Self::CANARY,
            Network::Enjin => Self::ENJIN,
        }
    }
}
