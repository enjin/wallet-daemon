use crate::SubstrateClient;
use hex_literal::hex;
use parity_scale_codec::Decode;
use std::fmt::Debug;
use std::sync::Arc;
use subxt::config::DefaultTransactionExtensions;
use subxt::config::substrate::SpecVersionForRange;
use subxt::utils::H256;
use subxt::{Config, Metadata, PolkadotConfig, SubstrateConfig};

#[derive(Debug, Clone)]
pub struct EnjinConfig {
    pub genesis_hash: H256,
    pub config: SubstrateConfig,
}

// TODO: only use these if the default doesn't work
// type EnjinTxExtensions<T> = (
//     transaction_extensions::VerifySignature<T>,
//     transaction_extensions::CheckSpecVersion,
//     transaction_extensions::CheckTxVersion,
//     transaction_extensions::CheckGenesis<T>,
//     transaction_extensions::CheckMortality<T>,
//     transaction_extensions::CheckMetadataHash,
//     transaction_extensions::CheckNonce,
//     transaction_extensions::ChargeTransactionPayment,
// );

impl Config for EnjinConfig {
    type AccountId = <PolkadotConfig as Config>::AccountId;
    type Address = <PolkadotConfig as Config>::Address;
    type Signature = <SubstrateConfig as Config>::Signature;
    type Header = <SubstrateConfig as Config>::Header;
    type TransactionExtensions = DefaultTransactionExtensions<EnjinConfig>;
    type AssetId = <SubstrateConfig as Config>::AssetId;
    type Hasher = <SubstrateConfig as Config>::Hasher;

    // Forward these methods to the default SubstrateConfig:
    fn genesis_hash(&self) -> Option<subxt::config::HashFor<Self>> {
        Some(self.genesis_hash)
    }
    fn legacy_types_for_spec_version<'this>(
        &'this self,
        spec_version: u32,
    ) -> Option<scale_info_legacy::TypeRegistrySet<'this>> {
        self.config.legacy_types_for_spec_version(spec_version)
    }
    fn metadata_for_spec_version(&self, spec_version: u32) -> Option<subxt::ArcMetadata> {
        self.config.metadata_for_spec_version(spec_version)
    }
    fn set_metadata_for_spec_version(&self, spec_version: u32, metadata: subxt::ArcMetadata) {
        self.config
            .set_metadata_for_spec_version(spec_version, metadata);
    }
    fn spec_and_transaction_version_for_block_number(
        &self,
        block_number: u64,
    ) -> Option<(u32, u32)> {
        self.config
            .spec_and_transaction_version_for_block_number(block_number)
    }
}

pub async fn setup_client() -> Arc<SubstrateClient> {
    let spec_version = 1031;
    let ranges = vec![SpecVersionForRange {
        block_range: 0..u64::MAX,
        spec_version,
        transaction_version: 12,
    }];

    let metadata_bytes = hex!("");

    let option: Option<Vec<u8>> = Decode::decode(&mut &metadata_bytes[..]).unwrap();
    let metadata_bytes = option.ok_or("No metadata returned").unwrap();

    let metadata = Arc::new(Metadata::decode_from(&metadata_bytes).unwrap());

    let genesis_hash = H256::from(hex!(
        "91b171bb158e2d3848fa23a9f1c25182fb8e20313b2c1eb49219da7a70ce90c3"
    ));
    let config = SubstrateConfig::builder()
        .set_spec_version_for_block_ranges(ranges)
        .set_metadata_for_spec_versions([(spec_version, metadata)])
        .set_genesis_hash(genesis_hash)
        .build();
    let config = EnjinConfig {
        genesis_hash,
        config,
    };
    let client = SubstrateClient::new_with_config(config);

    Arc::new(client)
}
