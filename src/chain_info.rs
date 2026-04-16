//! Retrieving data about the blockchain

use crate::graphql::{
    GetAccountNonce, GetChainInfo, GetCurrentBlockNumber, get_account_nonce, get_chain_info,
    get_current_block_number,
};
use crate::substrate_client::EnjinConfig;
use crate::types::{Chain, MetadataInfo, Network};
use crate::{SubstrateClient, global, utils};
use graphql_client::GraphQLQuery;
use hex_literal::hex;
use parity_scale_codec::Decode;
use std::sync::Arc;
use subxt::config::substrate::SpecVersionForRange;
use subxt::utils::H256;
use subxt::{Metadata, SubstrateConfig};
use subxt_signer::sr25519::PublicKey;

/// Fetch and insert the metadata for `network` and `chain`.
pub async fn update_metadata_and_substrate_client(
    network: Network,
    chain: Chain,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response_data = utils::execute_query::<GetChainInfo>(GetChainInfo::build_query(
        get_chain_info::Variables {
            network: network.into(),
            chain: chain.into(),
        },
    ))
    .await?;

    let info = response_data.result.ok_or("no result for metadata")?;

    if info.metadata_version > 16 {
        return Err("metadata version greater than 16 is not supported".into());
    }

    let metadata_bytes = hex::decode(info.metadata.split('x').nth(1).ok_or("missing 0x")?)?;
    let option: Option<Vec<u8>> = Decode::decode(&mut &metadata_bytes[..])?;
    let metadata_bytes = option.ok_or("No metadata returned")?;

    let metadata = Arc::new(Metadata::decode_from(&metadata_bytes)?);

    // create client
    let spec_version = info.spec_version as u32;
    let ranges = vec![SpecVersionForRange {
        block_range: 0..u64::MAX,
        spec_version,
        transaction_version: info.transaction_version as u32,
    }];
    let genesis_hash = get_genesis_hash(network, chain);
    let config = SubstrateConfig::builder()
        .set_spec_version_for_block_ranges(ranges)
        .set_metadata_for_spec_versions([(spec_version, metadata.clone())])
        .set_genesis_hash(genesis_hash)
        .build();
    let config = EnjinConfig {
        genesis_hash,
        config,
    };
    let client = SubstrateClient::new_with_config(config);

    global::insert_metadata(
        Network::Canary,
        Chain::Matrix,
        MetadataInfo {
            spec_version: info.spec_version as u32,
            metadata: metadata.clone(),
            client,
        },
    )
    .await;

    Ok(())
}

/// Get the current block and spec version from a graphql query
pub async fn get_block_and_spec_version(
    network: Network,
    chain: Chain,
) -> Result<(u32, H256, u32), Box<dyn std::error::Error + Send + Sync>> {
    let response_data = utils::execute_query::<GetCurrentBlockNumber>(
        GetCurrentBlockNumber::build_query(get_current_block_number::Variables {
            network: network.into(),
            chain: chain.into(),
        }),
    )
    .await?;

    let info = response_data.result.ok_or("no result for current block")?;

    let block_hash = info
        .current_block_hash
        .split('x')
        .nth(1)
        .ok_or("decode block hash failed")?;
    let block_hash = hex::decode(block_hash)?;
    Ok((
        info.current_block_number as u32,
        H256::from_slice(&block_hash),
        info.spec_version as u32,
    ))
}

/// Get the genesis hash for a network and chain
fn get_genesis_hash(network: Network, chain: Chain) -> H256 {
    H256::from(match (network, chain) {
        (Network::Canary, Chain::Relay) => {
            hex!("735d8773c63e74ff8490fee5751ac07e15bfe2b3b5263be4d683c48dbdfbcd15")
        }
        (Network::Canary, Chain::Matrix) => {
            hex!("a37725fd8943d2a524cb7ecc65da438f9fa644db78ba24dcd0003e2f95645e8f")
        }
        (Network::Enjin, Chain::Relay) => {
            hex!("d8761d3c88f26dc12875c00d3165f7d67243d56fc85b4cf19937601a7916e5a9")
        }
        (Network::Enjin, Chain::Matrix) => {
            hex!("3af4ff48ec76d2efc8476730f423ac07e25ad48f5f4c9dc39c778b164d808615")
        }
    })
}

pub async fn get_account_nonce(
    network: Network,
    chain: Chain,
    account: &PublicKey,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let response_data = utils::execute_query::<GetAccountNonce>(GetAccountNonce::build_query(
        get_account_nonce::Variables {
            network: network.into(),
            chain: chain.into(),
            address: format!("0x{}", hex::encode(account.0)),
        },
    ))
    .await?;

    Ok(response_data.result as u64)
}
