use crate::global;
use crate::graphql::{GetChainInfo, get_chain_info};
use crate::types::{Chain, MetadataInfo, Network};
use graphql_client::GraphQLQuery;
use parity_scale_codec::Decode;
use std::sync::Arc;
use subxt::Metadata;

/// Fetch and insert the metadata for `network` and `chain`
pub async fn update_metadata(
    network: Network,
    chain: Chain,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let query = GetChainInfo::build_query(get_chain_info::Variables {
        network: network.into(),
        chain: chain.into(),
    });

    let client = global::GRAPHQL_CLIENT.write().await;

    let response = client
        .post(global::platform_url())
        .headers(global::headers())
        .json(&query)
        .send()
        .await?;

    let response_body: graphql_client::Response<get_chain_info::ResponseData> =
        response.json().await?;

    let response_data = response_body.data.ok_or("no response data for metadata")?;
    let info = response_data.result.ok_or("no result for metadata")?;
    let metadata_bytes = hex::decode(info.metadata.split('x').nth(1).ok_or("missing 0x")?)?;
    let option: Option<Vec<u8>> = Decode::decode(&mut &metadata_bytes[..])?;
    let metadata_bytes = option.ok_or("No metadata returned")?;

    let metadata = Arc::new(Metadata::decode_from(&metadata_bytes)?);
    global::insert_metadata(
        Network::Canary,
        Chain::Matrix,
        MetadataInfo {
            spec_version: info.spec_version as u32,
            metadata: metadata.clone(),
        },
    )
    .await;

    // TODO: update client

    Ok(())
}
