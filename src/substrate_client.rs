use std::fmt::Debug;
use subxt::config::DefaultTransactionExtensions;
use subxt::utils::H256;
use subxt::{Config, PolkadotConfig, SubstrateConfig};

#[derive(Debug, Clone)]
pub struct EnjinConfig {
    pub genesis_hash: H256,
    pub config: SubstrateConfig,
}

// if there is a problem in the future, consider using enjin tx extension
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
