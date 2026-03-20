use sp_core::sr25519::Signature;
use sp_core::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use subxt::utils::{AccountId32, MultiAddress};

type BlockNumber = u32;

#[derive(Clone, Eq, Encode, Decode, PartialEq, Debug, DecodeWithMemTracking, MaxEncodedLen)]
pub struct CallIndex {
    pub pallet_index: u8,
    pub extrinsic_index: u8,
}

#[derive(Clone, Eq, Encode, Decode, PartialEq, Debug, DecodeWithMemTracking)]
pub struct InnerCall {
    pub call_index: CallIndex,
    pub args: Vec<u8>,
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
        // a call to fuel tanks dispatch system.remark with args 1, 2, 3
        let mut data = hex!("360500a82a0376985e4bdca417ceebc52499664ac78437e5ae074de72907a1b42b643e0000000000000c01020300").to_vec();
        // remove the settings arg
        data.pop();
        let tank_id = hex!("a82a0376985e4bdca417ceebc52499664ac78437e5ae074de72907a1b42b643e");
        let inner_call = InnerCall {
            call_index: CallIndex {
                pallet_index: 0,
                extrinsic_index: 0,
            },
            args: vec![1, 2, 3],
        };
        assert_eq!(inner_call.encode(), vec![0, 0, 12, 1, 2, 3]);
        assert_eq!(
            DispatchTx::decode(&mut &data[..]).unwrap(),
            DispatchTx {
                call_index: CallIndex {
                    pallet_index: 54,
                    extrinsic_index: 5
                },
                tank_id: MultiAddress::Id(AccountId32(tank_id)),
                rule_set_id: 0,
                inner_call,
            }
        );
    }
}
