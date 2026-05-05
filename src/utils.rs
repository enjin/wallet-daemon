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
    if tracing::enabled!(tracing::Level::DEBUG)
        && let Ok(json) = serde_json::to_string(&query_body)
    {
        tracing::debug!("Request body: {json}");
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
        tracing::debug!("Response Body: {:?}", &response_body);
        response_body.data.ok_or_else(|| {
            if let Some(errors) = response_body.errors {
                let error_messages = errors
                    .into_iter()
                    .map(|e| {
                        if let Some(extensions) = e.extensions {
                            if let Some(category) = extensions.get("category") {
                                if category == "validation" {
                                    let ext_str = extensions
                                        .into_iter()
                                        .filter(|(k, _)| k.as_str() != "category")
                                        .map(|(_k, v)| format!("{v}"))
                                        .collect::<Vec<_>>()
                                        .join(",");
                                    if !ext_str.is_empty() {
                                        format!("{}\n    Extensions: {}", &e.message, ext_str)
                                    } else {
                                        e.message
                                    }
                                } else {
                                    e.message
                                }
                            } else {
                                e.message
                            }
                        } else {
                            e.message
                        }
                    })
                    .collect::<Vec<_>>();
                let error_str = error_messages.join("\n- ");
                format!("The following error(s) occurred:\n- {}", error_str).into()
            } else {
                "no response data".into()
            }
        })
    } else {
        Err(format!(
            "Received invalid response with status code {}",
            response.status()
        )
        .into())
    }
}
