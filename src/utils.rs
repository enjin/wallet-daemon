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
    let printed_request_body = serde_json::to_string(&query_body);
    tracing::debug!("Request Body: {printed_request_body:?}");
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
        let printed_response_body = format!("Response Body: {:?}", &response_body);
        tracing::debug!("Response Body: {printed_response_body}");
        response_body
            .data
            .ok_or(format!("no response data - Request body: {printed_request_body:?}, Response body: {printed_response_body}").into())
    } else {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|err| format!("failed to read response body: {err}"));
        Err(format!("response not ok: status={status}, request body={printed_request_body:?}, response body={body}").into())
    }
}
