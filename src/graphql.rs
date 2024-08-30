use graphql_client::GraphQLQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/mark_and_list_pending_transactions.graphql",
    response_derives = "Debug"
)]
pub struct MarkAndListPendingTransactions;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/update_transaction.graphql",
    response_derives = "Debug"
)]
pub struct UpdateTransaction;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/update_user.graphql",
    response_derives = "Debug"
)]
pub struct UpdateUser;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/get_pending_wallets.graphql",
    response_derives = "Debug"
)]
pub struct GetPendingWallets;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/set_wallet_account.graphql",
    response_derives = "Debug"
)]
pub struct SetWalletAccount;
