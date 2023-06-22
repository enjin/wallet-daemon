//! Some additional type definitions
pub(crate) type SignedExtra<T> = crate::extra::DefaultExtra<T>;

pub(crate) type UncheckedExtrinsic<T> = subxt::UncheckedExtrinsic<T, SignedExtra<T>>;
pub type PairSigner<T, P> = subxt::PairSigner<T, SignedExtra<T>, P>;
