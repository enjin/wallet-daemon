use std::fmt;
use std::marker::PhantomData;
use subxt::Metadata;
use subxt::ext::frame_decode::extrinsics::ExtrinsicTypeInfo;
use subxt::ext::scale_decode::TypeResolver;
use subxt::ext::scale_decode::visitor::{
    DecodeAsTypeResult, DecodeError, TypeIdFor, Unexpected, Visitor, decode_with_visitor, types,
};
use subxt::ext::scale_value::{Composite, Primitive, Value, ValueDef, scale};

/// The deepest nesting of types accepted in a fuel tank call. Finding where a call ends recurses
/// once per level, so this bounds the stack a payload can use. Each batch wrapped around a call
/// adds three levels, so real calls stay far below it.
const MAX_NESTING: usize = 128;

/// The runtime reads `None` settings as the default, whose fields are all false or `None`: the
/// same as `Some` followed by zeroes. Each bool or option takes one byte of them, so only settings
/// with a field that has no known default anyway can run out.
const DEFAULT_SETTINGS: [u8; 256] = {
    let mut bytes = [0; 256];
    bytes[0] = 1;
    bytes
};

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

    // Find where each argument ends without building it. The Platform supplies the inner call,
    // so its lengths and nesting are trusted only as far as the bytes back them.
    let mut remaining = &tx[2..];
    let mut call = None;
    let mut settings = None;
    for field in &info.args {
        let start = tx.len() - remaining.len();
        decode_with_visitor(&mut remaining, field.id, metadata.types(), Measure::new())
            .map_err(|e| format!("decoding fuel tank {}: {e}", field.name))?;
        let range = start..tx.len() - remaining.len();
        match field.name.as_ref() {
            "call" => call = Some(range),
            "settings" => settings = Some((range, field.id)),
            _ => {}
        }
    }
    if !remaining.is_empty() {
        return Err("unexpected trailing bytes in fuel tank call".into());
    }
    let call = call.ok_or("fuel tank call is missing its inner call")?;
    let (settings_range, settings_type) =
        settings.ok_or("fuel tank call is missing dispatch settings")?;

    let mut fields = dispatch_settings(&tx[settings_range.clone()], settings_type, metadata)?;
    let signature_index = fields
        .iter()
        .position(|(name, _)| name == "signature")
        .ok_or("dispatch settings is missing its signature field")?;
    let mut encode_settings = |signature: [u8; 64]| {
        fields[signature_index].1 = Value::unnamed_variant(
            "Some",
            [Value::named_composite([
                ("signature", Value::from_bytes(signature)),
                ("expiry_block", Value::u128(expiration_block.into())),
            ])],
        );
        let settings = Value::unnamed_variant("Some", [Value::named_composite(fields.clone())]);
        let mut encoded = Vec::new();
        scale::encode_as_type(&settings, settings_type, metadata.types(), &mut encoded)
            .map_err(|e| format!("encoding fuel tank settings: {e}"))?;
        Ok::<_, String>(encoded)
    };
    // Encode a placeholder first, so settings that cannot be encoded fail before anything is signed.
    encode_settings([0; 64])?;

    let mut message = tx[call].to_vec();
    message.extend_from_slice(&public_key);
    message.extend_from_slice(&expiration_block.to_le_bytes());
    let encoded_settings = encode_settings(sign(&message))?;

    let mut payload = tx[..settings_range.start].to_vec();
    payload.extend_from_slice(&encoded_settings);
    payload.extend_from_slice(&tx[settings_range.end..]);
    Ok(payload)
}

/// The named fields of the dispatch settings, in metadata order. `None` is read as the runtime's
/// default; a field that does not default to false or `None` is refused rather than guessed.
fn dispatch_settings(
    encoded: &[u8],
    settings_type: u32,
    metadata: &Metadata,
) -> Result<Vec<(String, Value)>, String> {
    let decode = |mut bytes: &[u8]| {
        scale::decode_as_type(&mut bytes, settings_type, metadata.types())
            .map(Value::remove_context)
    };
    let mut settings = decode(encoded).map_err(|e| format!("decoding fuel tank settings: {e}"))?;
    let is_default = matches!(&settings.value, ValueDef::Variant(option) if option.name == "None");
    if is_default {
        settings = decode(&DEFAULT_SETTINGS)
            .map_err(|e| format!("dispatch settings have no known default: {e}"))?;
    }
    if let ValueDef::Variant(option) = settings.value
        && option.name == "Some"
        && let Composite::Unnamed(values) = option.values
        && let Ok(
            [
                Value {
                    value: ValueDef::Composite(Composite::Named(fields)),
                    ..
                },
            ],
        ) = <[Value; 1]>::try_from(values)
    {
        if is_default
            && let Some((name, _)) = fields
                .iter()
                .find(|(name, value)| name != "signature" && !is_zero(value))
        {
            return Err(format!(
                "dispatch settings field {name} has no known default"
            ));
        }
        return Ok(fields);
    }
    Err("invalid dispatch settings".into())
}

/// Whether `value` is what a zero byte decodes to as a bool or an option.
fn is_zero(value: &Value) -> bool {
    match &value.value {
        ValueDef::Primitive(Primitive::Bool(flag)) => !flag,
        ValueDef::Variant(option) => option.name == "None" && option.values.is_empty(),
        _ => false,
    }
}

/// Walks a value without building it, to find where it ends. Unlike decoding into a `Value`, this
/// allocates nothing from an untrusted length, and stops recursing at [`MAX_NESTING`].
struct Measure<R> {
    depth: usize,
    types: PhantomData<R>,
}

impl<R> Measure<R> {
    fn new() -> Self {
        Self {
            depth: 0,
            types: PhantomData,
        }
    }

    fn nested(&self) -> Self {
        Self {
            depth: self.depth + 1,
            types: PhantomData,
        }
    }
}

#[derive(Debug)]
enum MeasureError {
    Decode(DecodeError),
    TooDeep,
}

impl From<DecodeError> for MeasureError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl fmt::Display for MeasureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => error.fmt(f),
            Self::TooDeep => write!(f, "nested more than {MAX_NESTING} levels deep"),
        }
    }
}

/// Visit each item of a container one level deeper, so that a length longer than the data runs
/// out of input rather than allocating.
macro_rules! visit_items {
    ($($method:ident($container:ident)),* $(,)?) => {$(
        fn $method<'scale, 'resolver>(
            self,
            value: &mut types::$container<'scale, 'resolver, R>,
            _: TypeIdFor<Self>,
        ) -> Result<Self::Value<'scale, 'resolver>, Self::Error> {
            while let Some(item) = value.decode_item(self.nested()) {
                item?;
            }
            Ok(())
        }
    )*};
}

impl<R: TypeResolver> Visitor for Measure<R> {
    type Value<'scale, 'resolver> = ();
    type Error = MeasureError;
    type TypeResolver = R;

    fn unchecked_decode_as_type<'scale, 'resolver>(
        self,
        input: &mut &'scale [u8],
        _: TypeIdFor<Self>,
        _: &'resolver Self::TypeResolver,
    ) -> DecodeAsTypeResult<Self, Result<Self::Value<'scale, 'resolver>, Self::Error>> {
        if self.depth > MAX_NESTING {
            // After an error the decoder skips the rest of the value, recursing with no bound of
            // its own, so leave it nothing to skip.
            *input = &input[input.len()..];
            return DecodeAsTypeResult::Decoded(Err(MeasureError::TooDeep));
        }
        DecodeAsTypeResult::Skipped(self)
    }

    // The decoder bounds-checks primitives, strings and bit sequences itself.
    fn visit_unexpected<'scale, 'resolver>(
        self,
        _: Unexpected,
    ) -> Result<Self::Value<'scale, 'resolver>, Self::Error> {
        Ok(())
    }

    visit_items! {
        visit_sequence(Sequence),
        visit_composite(Composite),
        visit_tuple(Tuple),
        visit_array(Array),
    }

    fn visit_variant<'scale, 'resolver>(
        self,
        value: &mut types::Variant<'scale, 'resolver, R>,
        _: TypeIdFor<Self>,
    ) -> Result<Self::Value<'scale, 'resolver>, Self::Error> {
        let fields = value.fields();
        while let Some(field) = fields.decode_item(self.nested()) {
            field?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::load_metadata_from;
    use hex_literal::hex;
    use parity_scale_codec::{Compact, Decode, Encode};
    use scale_info::form::PortableForm;
    use scale_info::{Field, TypeDef};
    use std::sync::Arc;
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
    // System.remark with settings-looking bytes inside its arguments.
    const REMARK: &[u8] = &hex!("00001001000001");

    // Upgrade fixtures captured from finalized blocks on 2026-10-05, as metadata V16 like the
    // Platform fetches it:
    // canary-matrixchain, spec 1041:
    // 0x2dcde497d568733a453905ac647ddd390c56634326783892217897d2d9246d37
    // canary-relaychain, spec 1080:
    // 0x5da76a82a3e26e4449cefc0b65b33cd8b6f5a4ef640c780a53e6bbb28dd5e4ff
    // enjin-relaychain, spec 1070:
    // 0x6dafad750837d255689eb7cf358ec1622c0d803a429d27a76650ed231356840e
    const CURRENT_RUNTIMES: [&str; 2] = [
        "canary_matrix_1041_metadata.scale",
        "canary_relay_1080_metadata.scale",
    ];

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

    /// Sign `input` and check that only its settings changed: the expected flags, then a signature
    /// over the inner call, caller and expiry, then `after_signature` (the remaining fields, such as
    /// `create_account` on current runtimes).
    fn assert_signed(
        metadata: &Metadata,
        input: &[u8],
        inner: &[u8],
        expected_flags: (bool, bool),
        after_signature: &[u8],
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
        assert!(before.iter().any(|(name, _)| name == "settings"));
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
            assert_eq!(bytes, after_signature);
            assert!(sr25519::verify(
                &sr25519::Signature(signature),
                &expected_message,
                &signer.public_key()
            ));
        }
    }

    /// Check that `input` is refused without anything being signed, and return why.
    fn assert_refused(metadata: &Metadata, input: &[u8]) -> String {
        sign_dispatch(input, metadata, CALLER, EXPIRY, |_| {
            panic!("must not sign a refused call")
        })
        .unwrap_err()
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

    /// `REMARK` wrapped in `levels` single-call `Utility.batch`es.
    fn nested_batches(metadata: &Metadata, levels: usize) -> Vec<u8> {
        let batch = metadata
            .extrinsic_call_info_by_name("Utility", "batch")
            .unwrap();
        let mut call = REMARK.to_vec();
        for _ in 0..levels {
            let mut wrapped = vec![batch.pallet_index, batch.call_index, 4]; // one call
            wrapped.extend(call);
            call = wrapped;
        }
        call
    }

    /// A fixture whose type registry the test can change, to model a newer runtime.
    fn editable(fixture: &str) -> Metadata {
        Arc::into_inner(load_metadata_from(fixture)).unwrap()
    }

    /// The fields of the struct named `name` (e.g. `DispatchSettings`).
    fn fields_of<'a>(metadata: &'a mut Metadata, name: &str) -> &'a mut Vec<Field<PortableForm>> {
        let ty = metadata
            .types_mut()
            .types
            .iter_mut()
            .find(|ty| ty.ty.path.segments.last().is_some_and(|last| last == name))
            .unwrap();
        let TypeDef::Composite(composite) = &mut ty.ty.type_def else {
            panic!("{name} must be a struct");
        };
        &mut composite.fields
    }

    /// `fixture` with a field `added` appended to `DispatchSettings`, of the same type as the field
    /// `field` of the struct `of`.
    fn with_added_settings_field(fixture: &str, (of, field): (&str, &str)) -> Metadata {
        let mut metadata = editable(fixture);
        let mut added = fields_of(&mut metadata, of)
            .iter()
            .find(|existing| existing.name.as_deref() == Some(field))
            .unwrap()
            .clone();
        added.name = Some("added".into());
        fields_of(&mut metadata, "DispatchSettings").push(added);
        metadata
    }

    #[test]
    fn signs_reported_canary_payload_and_preserves_create_account() {
        let metadata = load_metadata_from("canary_matrix_1041_metadata.scale");
        assert_signed(&metadata, ORIGINAL, INNER, (false, false), &[1]);
    }

    #[test]
    fn supports_legacy_dispatch_and_dispatch_and_touch() {
        for fixture in [
            "enjin_matrix_metadata.scale",
            "canary_matrix_metadata.scale",
            "enjin_matrix_v16_metadata.scale",
            "enjin_relay_1070_metadata.scale",
        ] {
            let metadata = load_metadata_from(fixture);
            for name in ["dispatch", "dispatch_and_touch"] {
                for (settings, flags) in
                    [(vec![0], (false, false)), (vec![1, 1, 1, 0], (true, true))]
                {
                    let input = dispatch(&metadata, name, &7u32.to_le_bytes(), REMARK, &settings);
                    assert_signed(&metadata, &input, REMARK, flags, &[]);
                }
            }
        }
    }

    #[test]
    fn supports_current_optional_rule_sets_and_preserves_all_settings() {
        for fixture in CURRENT_RUNTIMES {
            let metadata = load_metadata_from(fixture);
            for rule_set in [vec![0], vec![1, 7, 0, 0, 0]] {
                for (settings, flags, create_account) in [
                    (vec![0], (false, false), 0),
                    (vec![1, 1, 1, 0, 0], (true, true), 0),
                    (vec![1, 0, 1, 0, 1], (false, true), 1),
                    (vec![1, 1, 1, 0, 1], (true, true), 1),
                ] {
                    let input = dispatch(&metadata, "dispatch", &rule_set, REMARK, &settings);
                    assert_signed(&metadata, &input, REMARK, flags, &[create_account]);
                }
            }
        }
    }

    #[test]
    fn replaces_existing_signature_without_appending_another() {
        for (fixture, rule_set, after_signature) in [
            ("enjin_matrix_metadata.scale", vec![0, 0, 0, 0], vec![]),
            (
                "canary_matrix_1041_metadata.scale",
                vec![1, 0, 0, 0, 0],
                vec![1],
            ),
        ] {
            let metadata = load_metadata_from(fixture);
            let inner = hex!("000000");
            let mut settings = vec![1, 1, 0, 1];
            settings.extend([3; 64]);
            settings.extend(42u32.to_le_bytes());
            settings.extend(&after_signature);
            let input = dispatch(&metadata, "dispatch", &rule_set, &inner, &settings);
            assert_signed(&metadata, &input, &inner, (true, false), &after_signature);
        }
    }

    #[test]
    fn signs_the_entire_inner_proxy_call() {
        let metadata = load_metadata_from("canary_matrix_1041_metadata.scale");
        let proxy = metadata
            .extrinsic_call_info_by_name("Proxy", "proxy")
            .unwrap();
        let mut inner = vec![proxy.pallet_index, proxy.call_index, 0];
        inner.extend(CALLER); // real: MultiAddress::Id
        inner.push(0); // force_proxy_type: None
        inner.extend(REMARK); // nested System.remark
        let input = dispatch(
            &metadata,
            "dispatch",
            &[1, 0, 0, 0, 0],
            &inner,
            &[1, 0, 0, 0, 1],
        );
        assert_signed(&metadata, &input, &inner, (false, false), &[1]);
    }

    #[test]
    fn rejects_malformed_payloads_before_signing() {
        let metadata = load_metadata_from("canary_matrix_1041_metadata.scale");
        let mut trailing = ORIGINAL.to_vec();
        trailing.extend([0, 0, 1, 99]);
        for input in [
            &[][..],
            &[54][..],
            &[54, 6][..], // dispatch_and_touch, which current runtimes removed
            &ORIGINAL[..ORIGINAL.len() - 1],
            trailing.as_slice(),
            &hex!("000000")[..],
        ] {
            assert_refused(&metadata, input);
        }
    }

    #[test]
    fn rejects_lengths_the_payload_does_not_contain() {
        // Decoding these into values allocated by the declared length first, which aborted the
        // process or panicked with a capacity overflow instead of returning an error.
        let metadata = load_metadata_from("canary_matrix_1041_metadata.scale");
        let fuel_tanks = metadata
            .extrinsic_call_info_by_name("FuelTanks", "dispatch")
            .unwrap();
        let mut inputs = Vec::new();
        for length in [u64::from(u32::MAX), 1 << 40, 1 << 62] {
            // A tank_id of MultiAddress::Raw claiming `length` bytes.
            let mut input = vec![fuel_tanks.pallet_index, fuel_tanks.call_index, 2];
            input.extend(Compact(length).encode());
            input.extend([0; 3]);
            inputs.push(input);
            // An inner System.remark claiming `length` bytes.
            let mut remark = vec![0, 0];
            remark.extend(Compact(length).encode());
            inputs.push(dispatch(&metadata, "dispatch", &[0], &remark, &[0]));
        }
        // A transfer encoded for the legacy layout (`rule_set_id: u32 = 1`), still pending when the
        // runtime upgrades: read with current metadata, its recipient becomes a remark's length.
        let transfer = metadata
            .extrinsic_call_info_by_name("Balances", "transfer_allow_death")
            .unwrap();
        for recipient_prefix in [hex!("13ffffffffffffff7f"), hex!("130000000000000001")] {
            let mut inner = vec![transfer.pallet_index, transfer.call_index, 0];
            inner.extend(recipient_prefix);
            inner.extend([0x11; 23]);
            inner.extend(Compact(10u128.pow(18)).encode());
            inputs.push(dispatch(
                &metadata,
                "dispatch",
                &1u32.to_le_bytes(),
                &inner,
                &[0],
            ));
        }
        for input in inputs {
            let error = assert_refused(&metadata, &input);
            assert!(error.contains("Not enough data"), "{error}");
        }
    }

    #[test]
    fn bounds_how_deeply_a_call_may_nest() {
        // Tokio gives each worker thread 2 MiB. The deepest call admitted must fit in that even
        // unoptimised, and anything deeper must be refused rather than overflow the stack.
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(|| {
                let metadata = load_metadata_from("canary_matrix_1041_metadata.scale");
                // The call starts at depth 0 and each batch adds three levels; the remark takes
                // three more for its pallet's calls, its argument, and the argument's bytes.
                let deepest = (MAX_NESTING - 3) / 3;
                let call = nested_batches(&metadata, deepest);
                let input = dispatch(&metadata, "dispatch", &[0], &call, &[0]);
                let signer = Keypair::from_secret_key([7; 32]).unwrap();
                let message = [&call[..], &CALLER, &EXPIRY.to_le_bytes()].concat();
                let output = sign_dispatch(&input, &metadata, CALLER, EXPIRY, |signed| {
                    assert_eq!(signed, message);
                    signer.sign(signed).0
                })
                .unwrap();
                // `decode_fields` builds values and recurses without bound, so check the bytes
                // directly: everything up to the trailing `None` settings is unchanged.
                let (unchanged, settings) = output.split_at(input.len() - 1);
                assert_eq!(unchanged, &input[..input.len() - 1]);
                assert_eq!(settings[..4], [1, 0, 0, 1]); // Some, both flags false, Some(signature)
                let signature = sr25519::Signature(settings[4..68].try_into().unwrap());
                assert!(sr25519::verify(&signature, &message, &signer.public_key()));
                assert_eq!(settings[68..], [&EXPIRY.to_le_bytes()[..], &[0]].concat());
                for levels in [deepest + 1, 10_000] {
                    let call = nested_batches(&metadata, levels);
                    let input = dispatch(&metadata, "dispatch", &[0], &call, &[0]);
                    let error = assert_refused(&metadata, &input);
                    assert!(error.contains("levels deep"), "{error}");
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn defaults_settings_fields_a_newer_runtime_adds() {
        for (fixture, rule_set, create_account) in [
            ("enjin_matrix_metadata.scale", &[0; 4][..], &[][..]),
            ("canary_matrix_1041_metadata.scale", &[0], &[0]),
        ] {
            // Without settings, an added bool or option takes its zero default.
            for copied in [
                ("DispatchSettings", "use_none_origin"),
                ("DispatchSettings", "signature"),
            ] {
                let metadata = with_added_settings_field(fixture, copied);
                let input = dispatch(&metadata, "dispatch", rule_set, REMARK, &[0]);
                let after_signature = [create_account, &[0]].concat();
                assert_signed(&metadata, &input, REMARK, (false, false), &after_signature);
            }
            // With settings, the added field keeps whatever the Platform sent.
            let metadata =
                with_added_settings_field(fixture, ("DispatchSettings", "use_none_origin"));
            let after_signature = [create_account, &[1]].concat();
            let settings = [&[1, 1, 0, 0][..], &after_signature].concat();
            let input = dispatch(&metadata, "dispatch", rule_set, REMARK, &settings);
            assert_signed(&metadata, &input, REMARK, (true, false), &after_signature);
        }
    }

    #[test]
    fn refuses_settings_it_cannot_fill_in_before_signing() {
        let fixture = "canary_matrix_1041_metadata.scale";

        // An added field without a zero default (here a u32) cannot be filled in for `None`, but
        // is kept as sent when the Platform provides settings.
        let metadata = with_added_settings_field(fixture, ("ExpirableSignature", "expiry_block"));
        let error = assert_refused(
            &metadata,
            &dispatch(&metadata, "dispatch", &[0], REMARK, &[0]),
        );
        assert!(error.contains("added has no known default"), "{error}");
        let after_signature = [&[0][..], &5u32.to_le_bytes()].concat();
        let settings = [&[1, 0, 0, 0][..], &after_signature].concat();
        let input = dispatch(&metadata, "dispatch", &[0], REMARK, &settings);
        assert_signed(&metadata, &input, REMARK, (false, false), &after_signature);

        // Without a signature field, or with one of a type the daemon cannot fill, every dispatch
        // is refused, whether or not it has settings.
        let mut without_signature = editable(fixture);
        fields_of(&mut without_signature, "DispatchSettings")
            .retain(|field| field.name.as_deref() != Some("signature"));
        let mut retyped_signature = editable(fixture);
        let fields = fields_of(&mut retyped_signature, "DispatchSettings");
        let flag_type = fields[0].ty;
        for field in fields.iter_mut() {
            if field.name.as_deref() == Some("signature") {
                field.ty = flag_type;
            }
        }
        for (metadata, settings) in [
            (&without_signature, vec![1, 0, 0, 0]),
            (&retyped_signature, vec![1, 0, 0, 0, 0]),
        ] {
            for settings in [vec![0], settings] {
                assert_refused(
                    metadata,
                    &dispatch(metadata, "dispatch", &[0], REMARK, &settings),
                );
            }
        }
    }
}
