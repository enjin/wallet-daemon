use async_trait::async_trait;
use reqwest::{Request, Response};

#[async_trait]
pub trait RequestExecutor {
    async fn execute(&self, request: Request) -> Result<Response, reqwest::Error>;
}

#[async_trait]
impl RequestExecutor for reqwest::Client {
    // We don't test connections
    #[cfg(not(tarpaulin_include))]
    async fn execute(&self, request: Request) -> Result<Response, reqwest::Error> {
        reqwest::Client::execute(self, request).await
    }
}
