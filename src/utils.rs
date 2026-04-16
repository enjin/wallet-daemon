use crate::global;
use backon::{ExponentialBuilder, Retryable};

pub async fn execute_query<T>(
    query_body: graphql_client::QueryBody<T::Variables>,
) -> Result<T::ResponseData, Box<dyn std::error::Error + Send + Sync>>
where
    T: graphql_client::GraphQLQuery,
    T::Variables: serde::Serialize,
{
    // println!("query_body: {:?}", serde_json::to_string(&query_body));
    let client = global::graphql_client().await;

    let response = client
        .post(global::platform_url())
        .headers(global::headers())
        .json(&query_body)
        .send()
        .await?;

    let response_body: graphql_client::Response<T::ResponseData> = response.json().await?;

    response_body.data.ok_or("no response data".into())
}

pub async fn execute_query_with_retry<T>(
    query: graphql_client::QueryBody<T::Variables>,
    retry_strategy: ExponentialBuilder,
) -> Result<T::ResponseData, Box<dyn std::error::Error + Send + Sync>>
where
    T: graphql_client::GraphQLQuery,
    T::Variables: serde::Serialize,
{
    let client = global::graphql_client().await;

    let response = (|| async {
        client
            .post(global::platform_url())
            .headers(global::headers())
            .json(&query)
            .send()
            .await
    })
    .retry(retry_strategy)
    .await?;

    let response_body: graphql_client::Response<T::ResponseData> = response.json().await?;

    response_body.data.ok_or("no response data".into())
}
