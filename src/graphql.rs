use graphql_client::GraphQLQuery;

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
