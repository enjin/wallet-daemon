use parity_scale_codec::Encode;
use subxt::Metadata;
use subxt::ext::frame_decode::extrinsics::ExtrinsicTypeInfo;
use subxt::ext::scale_value::{Composite, Value, ValueDef, scale};

/// Sign the fuel tank's inner call and replace its signature using the chain's call layout.
/// In particular, neither the rule-set ID nor the trailing dispatch settings have a fixed size.
/// Metadata selects the legacy layout or the one introduced in Relaychain 1080 / Matrixchain 1040,
/// independently for each network and chain.
pub fn sign_dispatch(
    tx: &[u8],
    metadata: &Metadata,
    public_key: [u8; 32],
    expiration_block: u32,
    sign: impl FnOnce(&[u8]) -> [u8; 64],
) -> Result<Vec<u8>, String> {
    let [pallet_index, call_index, ..] = tx else {
        return Err("fuel tank call is missing its call index".into());
    };
    let info = metadata
        .extrinsic_call_info_by_index(*pallet_index, *call_index)
        .map_err(|e| format!("fuel tank call metadata: {e}"))?;
    if info.pallet_name != "FuelTanks"
        || !matches!(info.call_name.as_ref(), "dispatch" | "dispatch_and_touch")
    {
        return Err("expected FuelTanks.dispatch or FuelTanks.dispatch_and_touch".into());
    }

    let mut remaining = &tx[2..];
    let mut fields = Vec::new();
    let mut inner_call = None;
    let mut settings = None;
    for field in &info.args {
        let before = remaining;
        let value = scale::decode_as_type(&mut remaining, field.id, metadata.types())
            .map_err(|e| format!("decoding fuel tank {}: {e}", field.name))?;
        let encoded = before[..before.len() - remaining.len()].to_vec();
        match field.name.as_ref() {
            "call" => inner_call = Some(encoded.clone()),
            "settings" => settings = Some((fields.len(), field.id, value.remove_context())),
            _ => {}
        }
        fields.push(encoded);
    }
    if !remaining.is_empty() {
        return Err("unexpected trailing bytes in fuel tank call".into());
    }
    let mut message = inner_call.ok_or("fuel tank call is missing its inner call")?;
    let (settings_index, settings_type, mut settings) =
        settings.ok_or("fuel tank call is missing dispatch settings")?;
    message.extend_from_slice(&public_key);
    message.extend_from_slice(&expiration_block.encode());

    let signature = Value::unnamed_variant(
        "Some",
        [Value::named_composite([
            ("signature", Value::from_bytes(sign(&message))),
            ("expiry_block", Value::u128(expiration_block.into())),
        ])],
    );
    match &mut settings.value {
        ValueDef::Variant(option) if option.name == "None" => {
            // Named encoding selects the fields present in metadata, so create_account is
            // included only on runtimes that support it. None previously implied defaults.
            settings = Value::unnamed_variant(
                "Some",
                [Value::named_composite([
                    ("use_none_origin", Value::bool(false)),
                    ("pays_remaining_fee", Value::bool(false)),
                    ("signature", signature),
                    ("create_account", Value::bool(false)),
                ])],
            );
        }
        ValueDef::Variant(option) if option.name == "Some" => {
            let Composite::Unnamed(values) = &mut option.values else {
                return Err("invalid dispatch settings option".into());
            };
            let [
                Value {
                    value: ValueDef::Composite(Composite::Named(fields)),
                    ..
                },
            ] = values.as_mut_slice()
            else {
                return Err("invalid dispatch settings fields".into());
            };
            let (_, existing_signature) =
                fields
                    .iter_mut()
                    .find(|(name, _)| name == "signature")
                    .ok_or("dispatch settings is missing its signature field")?;
            *existing_signature = signature;
        }
        _ => return Err("invalid dispatch settings".into()),
    }

    let mut encoded_settings = Vec::new();
    scale::encode_as_type(
        &settings,
        settings_type,
        metadata.types(),
        &mut encoded_settings,
    )
    .map_err(|e| format!("encoding fuel tank settings: {e}"))?;
    fields[settings_index] = encoded_settings;
    let mut payload = tx[..2].to_vec();
    payload.extend(fields.into_iter().flatten());
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_literal::hex;
    use parity_scale_codec::Decode;
    use subxt_signer::sr25519::{self, Keypair};

    const CALLER: [u8; 32] =
        hex!("d8b96830921c8e9e027c0dca4a7e5d14d02308cbfd2361732b7aeb4fb9d16773");
    const EXPIRY: u32 = 13_143_648;
    const INNER: &[u8] = &hex!(
        "0a0300d8b96830921c8e9e027c0dca4a7e5d14d02308cbfd2361732b7aeb4fb9d167731300008a5d78456301"
    );
    // Original Platform payload, before the old daemon's payload.pop().
    const ORIGINAL: &[u8] = &hex!(
        "3605000cd36b073e92cfd50ac15e013227822d81dc65e6307331321b1e927b436c02b901000000000a0300d8b96830921c8e9e027c0dca4a7e5d14d02308cbfd2361732b7aeb4fb9d167731300008a5d784563010100000001"
    );

    // Upgrade fixtures captured from finalized blocks on 2026-10-05:
    // rpc.matrix.canary.enjin.io, spec 1041:
    // 0x2dcde497d568733a453905ac647ddd390c56634326783892217897d2d9246d37
    // rpc.relay.canary.enjin.io, spec 1080:
    // 0x5da76a82a3e26e4449cefc0b65b33cd8b6f5a4ef640c780a53e6bbb28dd5e4ff
    // rpc.relay.blockchain.enjin.io, spec 1070:
    // 0x6dafad750837d255689eb7cf358ec1622c0d803a429d27a76650ed231356840e
    fn metadata(name: &str) -> Metadata {
        let bytes = std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        Metadata::decode_from(&bytes).unwrap()
    }

    // Decode with runtime metadata independently of the signing helper, and reject any trailing data.
    fn decode_fields(metadata: &Metadata, bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let info = metadata
            .extrinsic_call_info_by_index(bytes[0], bytes[1])
            .unwrap();
        let mut remaining = &bytes[2..];
        let fields = info
            .args
            .iter()
            .map(|field| {
                let before = remaining;
                scale::decode_as_type(&mut remaining, field.id, metadata.types()).unwrap();
                (
                    field.name.to_string(),
                    before[..before.len() - remaining.len()].to_vec(),
                )
            })
            .collect();
        assert!(
            remaining.is_empty(),
            "encoded call must not contain trailing bytes"
        );
        fields
    }

    fn assert_signed(
        metadata: &Metadata,
        input: &[u8],
        inner: &[u8],
        expected_flags: (bool, bool, Option<bool>),
    ) {
        let signer = Keypair::from_secret_key([7; 32]).unwrap();
        let mut expected_message = inner.to_vec();
        expected_message.extend(CALLER);
        expected_message.extend(EXPIRY.to_le_bytes());
        let output = sign_dispatch(input, metadata, CALLER, EXPIRY, |message| {
            assert_eq!(message, expected_message);
            signer.sign(message).0
        })
        .unwrap();
        assert_eq!(&output[..2], &input[..2]);
        let before = decode_fields(metadata, input);
        let after = decode_fields(metadata, &output);
        for ((name, original), (after_name, signed)) in before.iter().zip(&after) {
            assert_eq!(name, after_name);
            if name != "settings" {
                assert_eq!(signed, original, "{name} must be preserved byte-for-byte");
                continue;
            }
            // Read the expected SCALE layout directly, rather than using the code under test.
            let mut bytes = signed.as_slice();
            assert_eq!(u8::decode(&mut bytes).unwrap(), 1); // Some(settings)
            assert_eq!(bool::decode(&mut bytes).unwrap(), expected_flags.0);
            assert_eq!(bool::decode(&mut bytes).unwrap(), expected_flags.1);
            assert_eq!(u8::decode(&mut bytes).unwrap(), 1); // Some(signature)
            let signature = <[u8; 64]>::decode(&mut bytes).unwrap();
            assert_eq!(u32::decode(&mut bytes).unwrap(), EXPIRY);
            if let Some(create_account) = expected_flags.2 {
                assert_eq!(bool::decode(&mut bytes).unwrap(), create_account);
            }
            assert!(bytes.is_empty());
            assert!(sr25519::verify(
                &sr25519::Signature(signature),
                &expected_message,
                &signer.public_key()
            ));
        }
    }

    fn dispatch(
        metadata: &Metadata,
        name: &str,
        rule_set: &[u8],
        inner: &[u8],
        settings: &[u8],
    ) -> Vec<u8> {
        let info = metadata
            .extrinsic_call_info_by_name("FuelTanks", name)
            .unwrap();
        let mut bytes = vec![info.pallet_index, info.call_index, 0]; // MultiAddress::Id
        bytes.extend([9; 32]);
        bytes.extend(rule_set);
        bytes.extend(inner);
        bytes.extend(settings);
        bytes
    }

    #[test]
    fn signs_reported_canary_payload_and_preserves_create_account() {
        let metadata = metadata("canary_matrix_1041_metadata.scale");
        assert_signed(&metadata, ORIGINAL, INNER, (false, false, Some(true)));
    }

    #[test]
    fn supports_legacy_dispatch_and_dispatch_and_touch() {
        for fixture in [
            "enjin_matrix_metadata.scale",
            "canary_matrix_metadata.scale",
            "enjin_matrix_v16_metadata.scale",
            "enjin_relay_1070_metadata.scale",
        ] {
            let metadata = metadata(fixture);
            // System.remark with settings-looking bytes inside its arguments.
            let inner = hex!("00001001000001");
            for name in ["dispatch", "dispatch_and_touch"] {
                for (settings, flags) in [
                    (vec![0], (false, false, None)),
                    (vec![1, 1, 1, 0], (true, true, None)),
                ] {
                    let input = dispatch(&metadata, name, &7u32.to_le_bytes(), &inner, &settings);
                    assert_signed(&metadata, &input, &inner, flags);
                }
            }
        }
    }

    #[test]
    fn supports_current_optional_rule_sets_and_preserves_all_settings() {
        let metadata = metadata("canary_matrix_1041_metadata.scale");
        let inner = hex!("00001001000001");
        for rule_set in [vec![0], vec![1, 7, 0, 0, 0]] {
            for (settings, flags) in [
                (vec![0], (false, false, Some(false))),
                (vec![1, 1, 1, 0, 0], (true, true, Some(false))),
                (vec![1, 0, 1, 0, 1], (false, true, Some(true))),
            ] {
                let input = dispatch(&metadata, "dispatch", &rule_set, &inner, &settings);
                assert_signed(&metadata, &input, &inner, flags);
            }
        }
    }

    #[test]
    fn replaces_existing_signature_without_appending_another() {
        for (fixture, rule_set, create_account) in [
            ("enjin_matrix_metadata.scale", vec![0, 0, 0, 0], None),
            (
                "canary_matrix_1041_metadata.scale",
                vec![1, 0, 0, 0, 0],
                Some(true),
            ),
        ] {
            let metadata = metadata(fixture);
            let inner = hex!("000000");
            let mut settings = vec![1, 1, 0, 1];
            settings.extend([3; 64]);
            settings.extend(42u32.to_le_bytes());
            if let Some(value) = create_account {
                settings.push(value as u8);
            }
            let input = dispatch(&metadata, "dispatch", &rule_set, &inner, &settings);
            assert_signed(&metadata, &input, &inner, (true, false, create_account));
        }
    }

    #[test]
    fn signs_the_entire_inner_proxy_call() {
        let metadata = metadata("canary_matrix_1041_metadata.scale");
        let proxy = metadata
            .extrinsic_call_info_by_name("Proxy", "proxy")
            .unwrap();
        let mut inner = vec![proxy.pallet_index, proxy.call_index, 0];
        inner.extend(CALLER); // real: MultiAddress::Id
        inner.push(0); // force_proxy_type: None
        inner.extend(hex!("00001001000001")); // nested System.remark
        let input = dispatch(
            &metadata,
            "dispatch",
            &[1, 0, 0, 0, 0],
            &inner,
            &[1, 0, 0, 0, 1],
        );
        assert_signed(&metadata, &input, &inner, (false, false, Some(true)));
    }

    #[test]
    fn supports_upgraded_relaychain_dispatch() {
        let metadata = metadata("canary_relay_1080_metadata.scale");
        let inner = hex!("00001001000001");
        for rule_set in [vec![0], vec![1, 7, 0, 0, 0]] {
            for (settings, flags) in [
                (vec![0], (false, false, Some(false))),
                (vec![1, 1, 1, 0, 1], (true, true, Some(true))),
            ] {
                let input = dispatch(&metadata, "dispatch", &rule_set, &inner, &settings);
                assert_signed(&metadata, &input, &inner, flags);
            }
        }
    }

    #[test]
    fn rejects_malformed_payloads_before_signing() {
        let metadata = metadata("canary_matrix_1041_metadata.scale");
        let mut trailing = ORIGINAL.to_vec();
        trailing.extend([0, 0, 1, 99]);
        for input in [
            &[][..],
            &[54][..],
            &ORIGINAL[..ORIGINAL.len() - 1],
            trailing.as_slice(),
            &hex!("000000")[..],
        ] {
            assert!(
                sign_dispatch(input, &metadata, CALLER, EXPIRY, |_| panic!(
                    "must not sign malformed input"
                ))
                .is_err()
            );
        }
    }
}
