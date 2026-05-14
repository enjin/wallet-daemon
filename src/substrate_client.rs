use std::fmt::Debug;
use subxt::config::DefaultTransactionExtensions;
use subxt::utils::H256;
use subxt::{Config, PolkadotConfig, SubstrateConfig};

#[derive(Debug, Clone)]
pub struct EnjinConfig {
    pub genesis_hash: H256,
    pub config: SubstrateConfig,
}

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

/// End-to-end signature verification.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_info::get_genesis_hash;
    use crate::transaction::payload::{RawFields, RawPayload};
    use crate::types::{Chain, Network};
    use hex_literal::hex;
    use parity_scale_codec::{Compact, Encode};
    use std::sync::Arc;
    use subxt::Metadata;
    use subxt::OfflineClient;
    use subxt::config::DefaultExtrinsicParamsBuilder;
    use subxt::config::substrate::SpecVersionForRange;
    use subxt::tx::Payload;
    use subxt::utils::Era;
    use subxt_signer::sr25519;

    fn enjin_matrix_genesis() -> [u8; 32] {
        get_genesis_hash(Network::Enjin, Chain::Matrix).0
    }

    fn canary_matrix_genesis() -> [u8; 32] {
        get_genesis_hash(Network::Canary, Chain::Matrix).0
    }

    // Matches `state_getRuntimeVersion` on both
    // https://rpc.matrix.blockchain.enjin.io and https://rpc.matrix.canary.enjin.io
    // at the time the fixture metadata was captured (both runtimes share
    // spec_version / transaction_version).
    const SPEC_VERSION: u32 = 1031;
    const TX_VERSION: u32 = 12;

    fn load_metadata_from(filename: &str) -> Arc<Metadata> {
        let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), filename);
        let bytes = std::fs::read(&path).expect("metadata fixture missing");
        Arc::new(Metadata::decode_from(&bytes).expect("decode metadata"))
    }

    fn load_enjin_matrix_v14_metadata() -> Arc<Metadata> {
        load_metadata_from("enjin_matrix_metadata.scale")
    }

    fn load_canary_matrix_metadata() -> Arc<Metadata> {
        load_metadata_from("canary_matrix_metadata.scale")
    }

    fn load_enjin_matrix_v16_metadata() -> Arc<Metadata> {
        load_metadata_from("enjin_matrix_v16_metadata.scale")
    }

    fn build_client(metadata: Arc<Metadata>, genesis: [u8; 32]) -> OfflineClient<EnjinConfig> {
        let genesis = H256::from(genesis);
        let ranges = vec![SpecVersionForRange {
            block_range: 0..u64::MAX,
            spec_version: SPEC_VERSION,
            transaction_version: TX_VERSION,
        }];
        let inner = SubstrateConfig::builder()
            .set_spec_version_for_block_ranges(ranges)
            .set_metadata_for_spec_versions([(SPEC_VERSION, metadata)])
            .set_genesis_hash(genesis)
            .build();
        let cfg = EnjinConfig {
            genesis_hash: genesis,
            config: inner,
        };
        OfflineClient::new_with_config(cfg)
    }

    /// Mortality parameters used when re-deriving the runtime's view of an
    /// extrinsic during verification.
    #[derive(Clone)]
    struct TestMortality {
        for_n_blocks: u64,
        from_block_n: u64,
        from_block_hash: H256,
    }

    /// Test-only parameters mirroring what the daemon feeds into
    /// `DefaultExtrinsicParamsBuilder` in production.
    #[derive(Clone)]
    struct TestParams {
        nonce: u64,
        tip: u128,
        mortality: TestMortality,
    }

    /// Sign a payload via the standard subxt path the daemon uses in
    /// production (`create_signable_offline` + `sign`).
    fn sign_with_subxt<P: Payload>(
        client: &OfflineClient<EnjinConfig>,
        block_number: u64,
        payload: &P,
        params: &TestParams,
        signer: &sr25519::Keypair,
    ) -> Vec<u8> {
        let subxt_params = DefaultExtrinsicParamsBuilder::<EnjinConfig>::new()
            .nonce(params.nonce)
            .tip(params.tip)
            .mortal_from_unchecked(
                params.mortality.for_n_blocks,
                params.mortality.from_block_n,
                params.mortality.from_block_hash,
            )
            .build();
        let at = client.at_block(block_number).expect("at_block");
        let mut tx = at
            .tx()
            .create_signable_offline(payload, subxt_params)
            .expect("create signable");
        let signed = tx.sign(signer).expect("sign");
        signed.into_encoded()
    }

    fn remark_payload(bytes: Vec<u8>) -> RawPayload {
        RawPayload {
            pallet_name: "System".to_string(),
            call_name: "remark".to_string(),
            field_bytes: RawFields(bytes),
        }
    }

    /// Decode the v4 envelope. Returns `(tail_bytes_with_extra_and_call, signature, address_pk)`.
    fn decode_v4_envelope(signed: &[u8]) -> (Vec<u8>, sr25519::Signature, [u8; 32]) {
        let mut s = signed;
        let _len: Compact<u64> =
            <Compact<u64> as parity_scale_codec::Decode>::decode(&mut s).expect("len prefix");

        let version = s[0];
        s = &s[1..];
        assert_eq!(version, 0x84, "expected signed v4 extrinsic");

        assert_eq!(s[0], 0x00, "expected MultiAddress::Id variant");
        s = &s[1..];
        let mut pubkey = [0u8; 32];
        pubkey.copy_from_slice(&s[..32]);
        s = &s[32..];

        assert_eq!(s[0], 0x01, "expected MultiSignature::Sr25519 variant");
        s = &s[1..];
        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(&s[..64]);
        let signature = sr25519::Signature(sig_bytes);
        s = &s[64..];

        (s.to_vec(), signature, pubkey)
    }

    /// Split the v4 tail emitted by subxt's `DefaultTransactionExtensions`
    /// into `(extra, call)`. Subxt's default extras layout (relevant subset)
    /// is: Era | CheckMetadataHash mode (1 byte) | CompactNonce | CompactTip.
    /// `CheckNonZeroSender`, `CheckSpecVersion`, `CheckTxVersion`,
    /// `CheckGenesis`, `CheckMortality`, `CheckWeight` all contribute either
    /// nothing or are accounted for inline by the encoder; what ends up in
    /// the wire `extra` is the four fields above.
    fn split_extra(tail: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let extra_start = tail;
        let mut p = tail;

        // Era: first byte 0 means Immortal (1 byte total), else mortal (2 bytes total).
        if p[0] == 0 {
            p = &p[1..];
        } else {
            p = &p[2..];
        }
        // CheckMetadataHash mode (1 byte)
        p = &p[1..];
        // CheckNonce: compact u64
        let _n: Compact<u64> =
            <Compact<u64> as parity_scale_codec::Decode>::decode(&mut p).expect("nonce");
        // ChargeTransactionPayment: compact u128
        let _t: Compact<u128> =
            <Compact<u128> as parity_scale_codec::Decode>::decode(&mut p).expect("tip");

        let consumed = extra_start.len() - p.len();
        (extra_start[..consumed].to_vec(), p.to_vec())
    }

    fn expected_extra(params: &TestParams) -> Vec<u8> {
        let mut e: Vec<u8> = Vec::new();
        Era::mortal(params.mortality.for_n_blocks, params.mortality.from_block_n).encode_to(&mut e);
        0u8.encode_to(&mut e); // CheckMetadataHash::Disabled
        Compact(params.nonce).encode_to(&mut e);
        Compact(params.tip).encode_to(&mut e);
        e
    }

    /// Build the runtime's implicit (additional signed) data. For both Enjin
    /// Matrix and Canary Matrix, the only implicit-carrying extensions are
    /// `CheckSpecVersion`, `CheckTxVersion`, `CheckGenesis`, `CheckMortality`,
    /// and `CheckMetadataHash`.
    fn expected_implicit(genesis: [u8; 32], era_block_hash: H256) -> Vec<u8> {
        let mut i: Vec<u8> = Vec::new();
        SPEC_VERSION.encode_to(&mut i);
        TX_VERSION.encode_to(&mut i);
        i.extend_from_slice(&genesis);
        i.extend_from_slice(&era_block_hash.0);
        None::<H256>.encode_to(&mut i); // CheckMetadataHash implicit
        i
    }

    fn try_verify(
        signed: &[u8],
        signer: &sr25519::Keypair,
        params: &TestParams,
        genesis: [u8; 32],
    ) -> bool {
        let (tail, signature, address_pk) = decode_v4_envelope(signed);
        assert_eq!(address_pk, signer.public_key().0, "address mismatch");

        let (extra, call) = split_extra(&tail);
        assert_eq!(
            extra,
            expected_extra(params),
            "Extras don't match what we expected to be emitted",
        );

        let implicit = expected_implicit(genesis, params.mortality.from_block_hash);

        let mut to_sign = Vec::with_capacity(call.len() + extra.len() + implicit.len());
        to_sign.extend_from_slice(&call);
        to_sign.extend_from_slice(&extra);
        to_sign.extend_from_slice(&implicit);

        let message: Vec<u8> = if to_sign.len() > 256 {
            sp_crypto_hashing::blake2_256(&to_sign).to_vec()
        } else {
            to_sign
        };
        sr25519::verify(&signature, &message, &signer.public_key())
    }

    #[test]
    fn matrix_signed_extrinsic_verifies() {
        let metadata = load_enjin_matrix_v14_metadata();
        let client = build_client(metadata, enjin_matrix_genesis());
        let signer = sr25519::dev::alice();
        let params = TestParams {
            nonce: 7,
            tip: 0,
            mortality: TestMortality {
                for_n_blocks: 64,
                from_block_n: 1000,
                from_block_hash: H256::from(hex!(
                    "0000000000000000000000000000000000000000000000000000000000000001"
                )),
            },
        };
        let payload = remark_payload((b"hello".to_vec()).encode());
        let signed = sign_with_subxt(&client, 1000, &payload, &params, &signer);
        assert!(
            try_verify(&signed, &signer, &params, enjin_matrix_genesis()),
            "Matrix signature failed to verify - extension layout drift?",
        );
    }

    #[test]
    fn canary_matrix_signed_extrinsic_verifies() {
        // Canary Matrix has the same TxExtension layout as Enjin Matrix
        // (see matrixchain/runtime/canary-matrix/src/lib.rs::TxExtension),
        // so subxt's default extensions must produce a runtime-verifiable
        // signature with the canary genesis + canary metadata.
        let metadata = load_canary_matrix_metadata();
        let client = build_client(metadata, canary_matrix_genesis());
        let signer = sr25519::dev::alice();
        let params = TestParams {
            nonce: 11,
            tip: 0,
            mortality: TestMortality {
                for_n_blocks: 64,
                from_block_n: 2000,
                from_block_hash: H256::from(hex!(
                    "00000000000000000000000000000000000000000000000000000000000000aa"
                )),
            },
        };
        let payload = remark_payload((b"canary".to_vec()).encode());
        let signed = sign_with_subxt(&client, 2000, &payload, &params, &signer);
        assert!(
            try_verify(&signed, &signer, &params, canary_matrix_genesis()),
            "Canary Matrix signature failed to verify - extension layout drift?",
        );
    }

    #[test]
    fn matrix_signature_fails_when_genesis_is_wrong() {
        // Build a client with a deliberately incorrect genesis. The daemon
        // commits to that wrong genesis in its signing payload, so external
        // reconstruction with the real genesis must fail to verify.
        let metadata = load_enjin_matrix_v14_metadata();
        let wrong_genesis = [0xAA; 32];
        let client = build_client(metadata, wrong_genesis);
        let signer = sr25519::dev::alice();
        let params = TestParams {
            nonce: 0,
            tip: 0,
            mortality: TestMortality {
                for_n_blocks: 64,
                from_block_n: 1,
                from_block_hash: H256::from(hex!(
                    "0000000000000000000000000000000000000000000000000000000000000001"
                )),
            },
        };
        let payload = remark_payload((b"x".to_vec()).encode());
        let signed = sign_with_subxt(&client, 1, &payload, &params, &signer);
        assert!(
            !try_verify(&signed, &signer, &params, enjin_matrix_genesis()),
            "Verification must fail when the runtime's genesis hash differs",
        );
    }

    /// Re-run the matrix signing flow but with V16 metadata in place of V14.
    #[test]
    fn matrix_signed_extrinsic_verifies_with_v16_metadata() {
        let metadata = load_enjin_matrix_v16_metadata();
        let client = build_client(metadata, enjin_matrix_genesis());
        let signer = sr25519::dev::alice();
        let params = TestParams {
            nonce: 7,
            tip: 0,
            mortality: TestMortality {
                for_n_blocks: 64,
                from_block_n: 1000,
                from_block_hash: H256::from(hex!(
                    "0000000000000000000000000000000000000000000000000000000000000001"
                )),
            },
        };
        let payload = remark_payload((b"hello".to_vec()).encode());
        let signed = sign_with_subxt(&client, 1000, &payload, &params, &signer);
        assert!(
            try_verify(&signed, &signer, &params, enjin_matrix_genesis()),
            "Matrix signature failed to verify with V16 metadata - extension \
             layout drift between V14 and V16 metadata?",
        );
    }

    fn era_period_from_raw(raw: u64) -> u64 {
        raw.checked_next_power_of_two()
            .unwrap_or(1 << 16)
            .clamp(4, 1 << 16)
    }

    fn era_phase(period: u64, current: u64) -> u64 {
        let quantize_factor = (period >> 12).max(1);
        (current % period) / quantize_factor * quantize_factor
    }

    fn era_birth(period: u64, phase: u64, current: u64) -> u64 {
        (current.max(phase) - phase) / period * period + phase
    }

    /// The era is built off (signing_block_n, signing_block_hash). If the chain's
    /// current head at submission time is more than `period` ahead, the
    /// runtime's re-derived `birth` block number is different from the
    /// daemon's signing block. The chain's `system.block_hash(birth)` is
    /// then a different hash, so the runtime's payload-reconstruction
    /// substitutes a different hash into the implicit and the signature
    /// fails.
    #[test]
    fn era_anchor_lag_produces_signature_mismatch() {
        const HISTORICAL_TX_MORTALITY_RAW: u64 = 64;
        let period = era_period_from_raw(HISTORICAL_TX_MORTALITY_RAW);
        assert_eq!(period, 64, "raw=64 should clamp to period=64");

        let signing_block_n: u64 = 1000;
        let daemon_phase = era_phase(period, signing_block_n);

        for lag_blocks in [0u64, 1, 10, 32, 63, 64, 65, 100, 200, 14_400] {
            let chain_current = signing_block_n + lag_blocks;
            let runtime_birth = era_birth(period, daemon_phase, chain_current);
            let matches = runtime_birth == signing_block_n;
            eprintln!(
                "period={period:>4} lag={lag_blocks:>5}  signing_n={signing_block_n}  \
                 runtime_birth={runtime_birth:>5}  matches={matches}",
            );

            if lag_blocks < period {
                assert_eq!(
                    runtime_birth, signing_block_n,
                    "Within the mortality window the runtime must re-derive \
                     the same birth block the daemon committed to.",
                );
            } else {
                assert_ne!(
                    runtime_birth, signing_block_n,
                    "Beyond the mortality window the runtime re-derives a \
                     different birth block; the runtime would look up \
                     system.block_hash(birth) which is NOT signing_block_hash, \
                     and signature verification would fail.",
                );
            }
        }
    }
}
