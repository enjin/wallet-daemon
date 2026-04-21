use crate::global;
use backon::{ExponentialBuilder, Retryable};
use reqwest::StatusCode;
use std::fmt::Debug;

/// Helper function to execute a graphql query
pub async fn execute_query<T: graphql_client::GraphQLQuery>(
    variables: T::Variables,
    retry_strategy: Option<ExponentialBuilder>,
) -> Result<T::ResponseData, Box<dyn std::error::Error + Send + Sync>>
where
    T::Variables: serde::Serialize,
    T::ResponseData: Debug,
{
    let query_body = T::build_query(variables);
    if let Ok(json) = serde_json::to_string(&query_body) {
        tracing::debug!("Request Body: {json}");
    }
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
    if response.status() == StatusCode::OK {
        let response_body: graphql_client::Response<T::ResponseData> = response.json().await?;
        tracing::debug!("Response Body: {response_body:?}");
        response_body.data.ok_or("no response data".into())
    } else {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|err| format!("failed to read response body: {err}"));
        Err(format!("response not ok: status={status}, body={body}").into())
    }
}
