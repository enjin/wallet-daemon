use graphql_client::GraphQLQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/get_account_nonce.graphql",
    response_derives = "Debug"
)]
pub struct GetAccountNonce;

// doesn't use all of the fields
mod truncated {
    #[derive(graphql_client::GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.graphql",
        query_path = "graphql/get_current_block_number.graphql",
        response_derives = "Debug"
    )]
    pub struct GetChainInfo;
}
pub use truncated::GetChainInfo as GetCurrentBlockNumber;
pub use truncated::get_chain_info as get_current_block_number;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/get_chain_info.graphql",
    response_derives = "Debug"
)]
pub struct GetChainInfo;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/get_pending_transactions.graphql",
    response_derives = "Debug"
)]
pub struct GetPendingTransactions;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/sign_transactions.graphql",
    response_derives = "Debug"
)]
pub struct SignTransactions;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/set_daemon_wallet_account.graphql",
    response_derives = "Debug,Copy,Clone"
)]
pub struct SetDaemonWalletAccount;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/get_pending_managed_wallet_creations.graphql",
    response_derives = "Debug"
)]
pub struct GetPendingManagedWalletCreations;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/populate_managed_wallets.graphql",
    response_derives = "Debug"
)]
pub struct PopulateManagedWallets;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/authenticate_pusher_socket.graphql",
    response_derives = "Debug"
)]
pub struct AuthenticatePusherSocket;
