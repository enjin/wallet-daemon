use parity_scale_codec::{Decode as CodecDecode, Error, Input};
use sp_core::sr25519::Signature;
use sp_core::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use subxt::utils::{AccountId32, MultiAddress};

type BlockNumber = u32;

#[derive(Clone, Eq, Encode, Decode, PartialEq, Debug, DecodeWithMemTracking, MaxEncodedLen)]
pub struct CallIndex {
    pub pallet_index: u8,
    pub extrinsic_index: u8,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct InnerCall {
    pub call_index: CallIndex,
    pub args: Vec<u8>,
}

impl Encode for InnerCall {
    fn encode(&self) -> Vec<u8> {
        let mut result = self.call_index.encode();
        result.extend_from_slice(&self.args);
        result
    }
}

impl Decode for InnerCall {
    fn decode<I: Input>(input: &mut I) -> Result<Self, Error> {
        let call_index = CallIndex {
            pallet_index: Decode::decode(input)?,
            extrinsic_index: Decode::decode(input)?,
        };
        let remaining = input.remaining_len()?.unwrap_or(0);
        let mut args = vec![0u8; remaining];
        input.read(&mut args)?;
        Ok(Self { call_index, args })
    }
}

#[derive(Clone, Eq, Encode, Decode, PartialEq, Debug)]
pub struct DispatchTx {
    pub call_index: CallIndex,
    pub tank_id: MultiAddress<AccountId32, u32>,
    pub rule_set_id: u32,
    pub inner_call: InnerCall,
}

#[derive(Clone, Eq, Encode, Decode, PartialEq, Debug, DecodeWithMemTracking, MaxEncodedLen)]
pub struct ExpirableSignature {
    pub signature: Signature,
    pub expiry_block: BlockNumber,
}

#[derive(Clone, Eq, PartialEq, Encode, Decode, MaxEncodedLen, DecodeWithMemTracking, Default)]
pub struct DispatchSettings {
    pub use_none_origin: bool,
    pub pays_remaining_fee: bool,
    pub signature: Option<ExpirableSignature>,
}

pub fn create_message(
    tx: &[u8],
    public_key: [u8; 32],
    expiration_block: u32,
) -> Result<Vec<u8>, ()> {
    let tx = match DispatchTx::decode(&mut &tx[..]) {
        Ok(x) => x,
        Err(e) => {
            tracing::error!("failed to decode dispatch settings: {e}");
            return Err(());
        }
    };

    tracing::info!(
        "fuel tank - creating message from inner_call: {}, public_key {}, expiration block: {}",
        hex::encode(tx.inner_call.encode()),
        hex::encode(public_key),
        expiration_block
    );
    let mut message = tx.inner_call.encode();
    message.extend_from_slice(&public_key);
    message.extend_from_slice(&expiration_block.encode());

    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_literal::hex;

    #[test]
    fn test_decode_dispatch_tx() {
        let tank_id = hex!("a82a0376985e4bdca417ceebc52499664ac78437e5ae074de72907a1b42b643e");
        let inner_call = InnerCall {
            call_index: CallIndex {
                pallet_index: 0,
                extrinsic_index: 0,
            },
            args: vec![1, 2, 3],
        };
        assert_eq!(inner_call.encode(), vec![0, 0, 1, 2, 3]);
        let tx = DispatchTx {
            call_index: CallIndex {
                pallet_index: 54,
                extrinsic_index: 5,
            },
            tank_id: MultiAddress::Id(AccountId32(tank_id)),
            rule_set_id: 0,
            inner_call,
        };

        let data = tx.encode();
        assert_eq!(DispatchTx::decode(&mut &data[..]).unwrap(), tx);

        let account = hex!("8eaf04151687736326c9fea17e25fc5287613693c912909cb226aa4794f26a48");
        let output = create_message(&data, account, 10).unwrap();
        let expected = hex!(
            "00000102038eaf04151687736326c9fea17e25fc5287613693c912909cb226aa4794f26a480a000000"
        );
        assert_eq!(output, expected);
    }
}
