//! Module that allows using a remote key stored and created in KMS
// Dead module(Left around just because it's easier than to excavate github later on)
use crate::wallet_trait::{KeyDerive, KeyDeriveError};
use crate::SignedExtra;
use crate::SignedPayload;
use crate::UncheckedExtrinsic;
use async_trait::async_trait;
use codec::Encode;
use rusoto_core::{Region, RusotoError};
use rusoto_kms::{
    CreateAliasError, CreateAliasRequest, CreateKeyError, CreateKeyRequest, GetPublicKeyError,
    GetPublicKeyRequest, KmsClient,
};
use rusoto_kms::{Kms, SignRequest};
use sp_application_crypto::ecdsa::Signature;
use sp_application_crypto::TryFrom;
use sp_core::crypto::Infallible;
use sp_core::{blake2_256, ecdsa};
use sp_runtime::{traits::IdentifyAccount, AccountId32, MultiSigner};
use std::fmt::Debug;
use subxt::Signer;
use thiserror::Error;

/// Signer for KMS keys
#[derive(Debug, Clone)]
pub struct KmsSigner<K: Kms, T: subxt::Config> {
    kms_client: K,
    key_alias: String,
    public_key: MultiSigner,
    account_id: T::AccountId,
}

/// Strategy for creating the key
#[derive(Debug, Clone)]
pub enum KeyStrategy {
    /// Will create the key
    Create,
    /// Will Retrieve the key
    #[allow(dead_code)]
    Retrieve,
}

/// Error for KMS functions
#[derive(Error, Debug)]
pub enum KmsError {
    /// One of the returned paramaters is empty
    #[error("One of the needed parameters was empty")]
    EmptyParameter(String),
    /// Can't create a signer for public key
    #[error("Compatibility error with KMS(Can't create signer from public key)")]
    CompatibilityError,
    // TODO: All this error could be better grouped into a single RusotoError
    /// Error creating key
    #[error("Problem with rusoto")]
    CreateError(#[from] rusoto_core::RusotoError<CreateKeyError>),
    /// Error getting public key
    #[error("Problem with rusoto")]
    PublicKeyError(#[from] rusoto_core::RusotoError<GetPublicKeyError>),
    /// Error aliasing key
    #[error("Problem with rusoto")]
    AliasError(#[from] rusoto_core::RusotoError<CreateAliasError>),
}

impl<T> KmsSigner<KmsClient, T>
where
    T: subxt::Config,
    AccountId32: Into<T::AccountId>,
{
    /// Constructs a new KMS signer
    pub async fn new(
        region: Region,
        strategy: KeyStrategy,
        key_name: String,
    ) -> Result<Self, KmsError> {
        let kms_client = KmsClient::new(region);

        match strategy {
            KeyStrategy::Create => {
                let key_response = kms_client
                    .create_key(CreateKeyRequest {
                        bypass_policy_lockout_safety_check: None,
                        custom_key_store_id: None,
                        customer_master_key_spec: Some("ECC_SECG_P256K1".to_string()),
                        description: Some("Efinity Account".to_string()),
                        key_usage: Some("SIGN_VERIFY".to_string()),
                        multi_region: Some(false),
                        origin: Some("AWS_KMS".to_string()),
                        policy: None, //TODO
                        tags: None,   //TODO
                    })
                    .await?;

                let target_key_id = key_response
                    .key_metadata
                    .ok_or_else(|| KmsError::EmptyParameter("key_metadata".to_string()))?
                    .key_id;
                let alias_response = kms_client
                    .create_alias(CreateAliasRequest {
                        alias_name: key_name.clone(),
                        target_key_id,
                    })
                    .await;
                // TODO: Ideally if the alias already exists we don't create another
                // but to prevent that we need to list all alias and iterate through them
                match alias_response {
                    Ok(_) => {}
                    Err(RusotoError::Service(CreateAliasError::AlreadyExists(_))) => {
                        tracing::warn!("Key with alias {} already existed", key_name)
                    }
                    _ => alias_response?,
                }
            }
            KeyStrategy::Retrieve => {}
        }

        let public_key = kms_client
            .get_public_key(GetPublicKeyRequest {
                grant_tokens: None,
                key_id: key_name.clone(),
            })
            .await?
            .public_key
            .ok_or_else(|| KmsError::EmptyParameter("pulic_key".to_string()))?;
        let pk_decoded = spki::SubjectPublicKeyInfo::try_from(&public_key[..]).unwrap();
        let public_key: MultiSigner = ecdsa::Public::from_full(pk_decoded.subject_public_key)
            .map_err(|_| KmsError::CompatibilityError)?
            .into();

        Ok(Self {
            kms_client,
            key_alias: key_name,
            public_key: public_key.clone(),
            account_id: public_key.into_account().into(),
        })
    }
}

#[async_trait]
impl<T, K> Signer<T, SignedExtra<T>> for KmsSigner<K, T>
where
    K: Kms + Send + Sync,
    T: subxt::Config,
    T::Signature: From<sp_core::ecdsa::Signature>,
    <T as subxt::Config>::AccountId: Clone,
    <T as subxt::Config>::AccountId: Into<<T as subxt::Config>::Address>,
    AccountId32: Into<T::AccountId>,
{
    async fn sign(&self, extrinsic: SignedPayload<T>) -> Result<UncheckedExtrinsic<T>, String> {
        // For some reason `encode_to` doesn't digest the message when it's less than 256 bytes this doesn't behave well with  Signature::Recovery
        // Since that always use blake 256 (Also KMS expects the message either already digested or not digested but we could work around that)
        // Still this seems like the best way unless we measure a big performance hit for hashing but we will cross that bridge when we get there.
        // (Probably if eventually we find an error with KMS signing is here)
        let deconstructed_extrinsic = extrinsic.deconstruct();
        let digest = blake2_256(&deconstructed_extrinsic.encode());
        let signature_bytes = self
            .kms_client
            .sign(SignRequest {
                grant_tokens: None,
                key_id: self.key_alias.clone(),
                message: digest.to_vec().into(),
                message_type: Some("DIGEST".to_string()),
                signing_algorithm: "ECDSA_SHA_256".to_string(),
            })
            .await
            .map_err(|e| format!("{}", e))?
            .signature
            .ok_or_else(|| "No signature returned".to_string())?;

        let sig = libsecp256k1::Signature::parse_der(&signature_bytes[..]).unwrap();

        // 😞 we need to do this to get the recovery id since KMS doesn't return that
        for i in 0..4 {
            tracing::trace!("Trying with recovery id: {}", i);
            let verify_id = libsecp256k1::RecoveryId::parse(i).unwrap();
            let sig = Signature::from((sig, verify_id));
            let recovered_public_key = sig.recover(&deconstructed_extrinsic.encode());
            if let Some(recovered_public_key) = recovered_public_key {
                let recovered_signer = MultiSigner::from(recovered_public_key);
                if recovered_signer == self.public_key {
                    let address: T::Address = self.account_id.clone().into();
                    let (call, extra, _) = deconstructed_extrinsic;
                    let extrinsic =
                        UncheckedExtrinsic::<T>::new_signed(call, address, sig.into(), extra);
                    return Ok(extrinsic);
                }
            }
        }

        Err("No Recovery ID worked with the returned signature".to_string())
    }

    fn account_id(&self) -> &T::AccountId {
        &self.account_id
    }

    fn nonce(&self) -> Option<T::Index> {
        // We don't use the `Signer` Nonce
        None
    }
}

impl<K, T> KeyDerive<T> for KmsSigner<K, T>
where
    K: Kms,
    T: subxt::Config,
{
    type DeriveError = KeyDeriveError<Infallible>;

    type Seed = [u8; 32];

    type NewSigner = KmsSigner<K, T>;

    #[allow(clippy::type_complexity)]
    fn derive(
        &self,
        _path: sp_application_crypto::DeriveJunction,
        _seed: Option<Self::Seed>,
        // The box here is to make it object safe
    ) -> Result<(Self::NewSigner, Option<Self::Seed>), Self::DeriveError> {
        Err(KeyDeriveError::NoKeyDerivation)
    }
}
