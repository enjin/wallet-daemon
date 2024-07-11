use codec::{Decode, Encode};
use derivative::Derivative;
use scale_info::TypeInfo;
use subxt::sp_runtime::{
    generic::Era, traits::DispatchInfoOf, transaction_validity::TransactionValidityError,
};
use subxt::storage::SignedExtension;
use subxt::SignedExtra;

use subxt::Config;

/// A version of [`std::marker::PhantomData`] that is also Send and Sync (which is fine
/// because regardless of the generic param, it is always possible to Send + Sync this
/// 0 size type).
#[derive(Derivative, Encode, Decode, scale_info::TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = ""),
    Default(bound = "")
)]
#[scale_info(skip_type_params(T))]
#[doc(hidden)]
pub struct PhantomDataSendSync<T>(core::marker::PhantomData<T>);

impl<T> PhantomDataSendSync<T> {
    pub(crate) fn new() -> Self {
        Self(core::marker::PhantomData)
    }
}

unsafe impl<T> Send for PhantomDataSendSync<T> {}
unsafe impl<T> Sync for PhantomDataSendSync<T> {}

/// Extra type.
// pub type Extra<T> = <<T as Config>::Extra as SignedExtra<T>>::Extra;

/// SignedExtra checks copied from substrate, in order to remove requirement to implement
/// substrate's `frame_system::Trait`

/// Ensure the runtime version registered in the transaction is the same as at present.
///
/// # Note
///
/// This is modified from the substrate version to allow passing in of the version, which is
/// returned via `additional_signed()`.

/// Ensure the runtime version registered in the transaction is the same as at present.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct CheckSpecVersion<T: Config>(
    pub PhantomDataSendSync<T>,
    /// Local version to be used for `AdditionalSigned`
    #[codec(skip)]
    pub u32,
);

impl<T: Config> SignedExtension for CheckSpecVersion<T> {
    const IDENTIFIER: &'static str = "CheckSpecVersion";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = u32;
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(self.1)
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Ensure the transaction version registered in the transaction is the same as at present.
///
/// # Note
///
/// This is modified from the substrate version to allow passing in of the version, which is
/// returned via `additional_signed()`.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct CheckTxVersion<T: Config>(
    pub PhantomDataSendSync<T>,
    /// Local version to be used for `AdditionalSigned`
    #[codec(skip)]
    pub u32,
);

impl<T: Config> SignedExtension for CheckTxVersion<T> {
    const IDENTIFIER: &'static str = "CheckTxVersion";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = u32;
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(self.1)
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Check metadata hash
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct CheckMetadataHash<T: Config>(
    /// The default structure for the Extra encoding
    pub u8,
    /// Metadata hash to be used for `AdditionalSigned`
    #[codec(skip)]
    pub Option<T::Hash>,
);

impl<T: Config> SignedExtension for crate::extra::CheckMetadataHash<T> {
    const IDENTIFIER: &'static str = "CheckMetadataHash";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = Option<T::Hash>;
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(None)
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Check genesis hash
///
/// # Note
///
/// This is modified from the substrate version to allow passing in of the genesis hash, which is
/// returned via `additional_signed()`.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct CheckGenesis<T: Config>(
    pub PhantomDataSendSync<T>,
    /// Local genesis hash to be used for `AdditionalSigned`
    #[codec(skip)]
    pub T::Hash,
);

impl<T: Config> SignedExtension for CheckGenesis<T> {
    const IDENTIFIER: &'static str = "CheckGenesis";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = T::Hash;
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(self.1)
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Check for transaction mortality.
///
/// # Note
///
/// This is modified from the substrate version to allow passing in of the genesis hash, which is
/// returned via `additional_signed()`. It assumes therefore `Era::Immortal` (The transaction is
/// valid forever)
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct CheckMortality<T: Config>(
    /// The default structure for the Extra encoding
    pub (Era, PhantomDataSendSync<T>),
    /// Local genesis hash to be used for `AdditionalSigned`
    #[codec(skip)]
    pub T::Hash,
);

impl<T: Config> SignedExtension for CheckMortality<T> {
    const IDENTIFIER: &'static str = "CheckMortality";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = T::Hash;
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(self.1)
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Nonce check and increment to give replay protection for transactions.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct CheckNonce<T: Config>(#[codec(compact)] pub T::Index);

impl<T: Config> SignedExtension for CheckNonce<T> {
    const IDENTIFIER: &'static str = "CheckNonce";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = ();
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(())
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Resource limit check.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct CheckWeight<T: Config>(pub PhantomDataSendSync<T>);

impl<T: Config> SignedExtension for CheckWeight<T> {
    const IDENTIFIER: &'static str = "CheckWeight";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = ();
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(())
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Require the transactor pay for themselves and maybe include a tip to gain additional priority
/// in the queue.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = ""),
    Default(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct ChargeTransactionPayment<T: Config>(#[codec(compact)] u128, pub PhantomDataSendSync<T>);

impl<T: Config> SignedExtension for ChargeTransactionPayment<T> {
    const IDENTIFIER: &'static str = "ChargeTransactionPayment";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = ();
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(())
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Require the transactor pay for themselves and maybe include a tip to gain additional priority
/// in the queue.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = ""),
    Default(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct ChargeAssetTxPayment<T: Config> {
    /// The tip for the block author.
    #[codec(compact)]
    pub tip: u128,
    /// The asset with which to pay the tip.
    pub asset_id: Option<u32>,
    /// Marker for unused type parameter.
    pub marker: PhantomDataSendSync<T>,
}

impl<T: Config> SignedExtension for ChargeAssetTxPayment<T> {
    const IDENTIFIER: &'static str = "ChargeAssetTxPayment";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = ();
    type Pre = ();
    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        Ok(())
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// Default `SignedExtra` for substrate runtimes.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(
    Clone(bound = ""),
    PartialEq(bound = ""),
    Debug(bound = ""),
    Eq(bound = "")
)]
#[scale_info(skip_type_params(T))]
pub struct DefaultExtraWithTxPayment<T: Config, X> {
    spec_version: u32,
    tx_version: u32,
    nonce: T::Index,
    genesis_hash: T::Hash,
    marker: PhantomDataSendSync<X>,
    current: u64,
    period: u64,
    block_hash: T::Hash,
    metadata_hash: u8,
}

impl<T, X> SignedExtra<T> for DefaultExtraWithTxPayment<T, X>
where
    T: Config,
    X: SignedExtension<AccountId = T::AccountId, Call = ()> + Default,
{
    #[allow(clippy::type_complexity)]
    type Extra = (
        CheckSpecVersion<T>,
        CheckTxVersion<T>,
        CheckGenesis<T>,
        CheckMortality<T>,
        CheckNonce<T>,
        CheckWeight<T>,
        CheckMetadataHash<T>,
        X,
    );

    type Parameters = (u64, u64, T::Hash);

    fn new(
        spec_version: u32,
        tx_version: u32,
        nonce: T::Index,
        genesis_hash: T::Hash,
        (current, period, block_hash): Self::Parameters,
    ) -> Self {
        DefaultExtraWithTxPayment {
            spec_version,
            tx_version,
            nonce,
            genesis_hash,
            marker: PhantomDataSendSync::new(),
            current,
            period,
            block_hash,
            metadata_hash: 0,
        }
    }

    fn extra(&self) -> Self::Extra {
        let current = self.current;
        let period = self.period;
        let era = Era::mortal(period, current);
        tracing::debug!("Current block number is {current} and period is {period}");
        tracing::debug!(
            "Era birth is  {} and era death is {}",
            era.birth(current),
            era.death(current)
        );
        (
            CheckSpecVersion(PhantomDataSendSync::new(), self.spec_version),
            CheckTxVersion(PhantomDataSendSync::new(), self.tx_version),
            CheckGenesis(PhantomDataSendSync::new(), self.genesis_hash),
            CheckMortality(
                (
                    Era::mortal(self.period, self.current),
                    PhantomDataSendSync::new(),
                ),
                self.block_hash,
            ),
            CheckNonce(self.nonce),
            CheckWeight(PhantomDataSendSync::new()),
            CheckMetadataHash(0, None),
            X::default(),
        )
    }
}

impl<T, X: SignedExtension<AccountId = T::AccountId, Call = ()> + Default> SignedExtension
    for DefaultExtraWithTxPayment<T, X>
where
    T: Config,
    X: SignedExtension,
{
    const IDENTIFIER: &'static str = "DefaultExtra";
    type AccountId = T::AccountId;
    type Call = ();
    type AdditionalSigned = <<Self as SignedExtra<T>>::Extra as SignedExtension>::AdditionalSigned;
    type Pre = ();

    fn additional_signed(&self) -> Result<Self::AdditionalSigned, TransactionValidityError> {
        self.extra().additional_signed()
    }
    fn pre_dispatch(
        self,
        _who: &Self::AccountId,
        _call: &Self::Call,
        _info: &DispatchInfoOf<Self::Call>,
        _len: usize,
    ) -> Result<Self::Pre, TransactionValidityError> {
        Ok(())
    }
}

/// A default `SignedExtra` configuration, with [`ChargeTransactionPayment`] for tipping.
///
/// Note that this must match the `SignedExtra` type in the target runtime's extrinsic definition.
pub type DefaultExtra<T> = DefaultExtraWithTxPayment<T, ChargeTransactionPayment<T>>;
