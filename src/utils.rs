use crate::global;
use backon::{ExponentialBuilder, Retryable};

/// Helper function to execute a graphql query
pub async fn execute_query<T: graphql_client::GraphQLQuery>(
    variables: T::Variables,
    retry_strategy: Option<ExponentialBuilder>,
) -> Result<T::ResponseData, Box<dyn std::error::Error + Send + Sync>>
where
    T::Variables: serde::Serialize,
{
    let query_body = T::build_query(variables);
    // println!("query_body: {:?}", serde_json::to_string(&query_body));
    let client = global::graphql_client().await;

    let response = if let Some(strategy) = retry_strategy {
        (|| async {
            client
                .post(global::platform_url())
                .headers(global::headers())
                .json(&query_body)
                .send()
                .await
        })
        .retry(strategy)
        .await?
    } else {
        client
            .post(global::platform_url())
            .headers(global::headers())
            .json(&query_body)
            .send()
            .await?
    };

    let response_body: graphql_client::Response<T::ResponseData> = response.json().await?;

    response_body.data.ok_or("no response data".into())
}
