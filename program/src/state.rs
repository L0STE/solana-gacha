//! Program-owned account views. Every `unsafe` needed to overlay a layout on
//! account bytes lives here; handlers only see checked views.

use crate::{constants::*, errors::GachaError};
pub use gacha_core::state::{Item, Pool, Pull};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError};

#[cold]
fn cold_marker() {}

#[inline(always)]
fn unlikely(b: bool) -> bool {
    if b {
        cold_marker();
    }
    b
}

pub trait Load: Sized {
    /// View the account after checking owner, length and version. The program
    /// never holds a checked borrow, so callers must not alias a mutable view
    /// of the same account.
    fn load(account: &AccountInfo) -> Result<&Self, ProgramError>;

    /// Same checks as `load`, returning a mutable view.
    #[allow(clippy::mut_from_ref)]
    fn load_mut(account: &AccountInfo) -> Result<&mut Self, ProgramError>;

    /// View an account this instruction just created: program-owned, exactly
    /// sized and still zeroed, so `set_inner` is the only valid next step.
    #[allow(clippy::mut_from_ref)]
    fn load_new(account: &AccountInfo) -> Result<&mut Self, ProgramError>;
}

macro_rules! load {
    ($name:ident, $len:expr, $version:expr, $valid_len:expr) => {
        impl Load for $name {
            #[inline(always)]
            fn load(account: &AccountInfo) -> Result<&Self, ProgramError> {
                if unlikely(!account.is_owned_by(&crate::ID)) {
                    return Err(GachaError::InvalidAccountOwner.into());
                }
                if unlikely(account.data_len() < $len) {
                    return Err(GachaError::InvalidAccountLength.into());
                }
                // SAFETY: length checked above; all fields have alignment 1.
                let this = unsafe { $name::from_bytes_unchecked(account.borrow_data_unchecked()) };
                if unlikely(this.version() != $version) {
                    return Err(GachaError::InvalidVersion.into());
                }
                if unlikely(!($valid_len)(this, account.data_len())) {
                    return Err(GachaError::InvalidAccountLength.into());
                }
                Ok(this)
            }

            #[inline(always)]
            fn load_mut(account: &AccountInfo) -> Result<&mut Self, ProgramError> {
                Self::load(account)?;
                // SAFETY: same layout guarantees as `load`; the caller holds no other view.
                Ok(unsafe { $name::from_bytes_unchecked_mut(account.borrow_mut_data_unchecked()) })
            }

            #[inline(always)]
            fn load_new(account: &AccountInfo) -> Result<&mut Self, ProgramError> {
                if unlikely(!account.is_owned_by(&crate::ID)) {
                    return Err(GachaError::InvalidAccountOwner.into());
                }
                if unlikely(account.data_len() != $len) {
                    return Err(GachaError::InvalidAccountLength.into());
                }
                // SAFETY: length checked above; all fields have alignment 1.
                let this =
                    unsafe { $name::from_bytes_unchecked_mut(account.borrow_mut_data_unchecked()) };
                if unlikely(this.version() != 0) {
                    return Err(GachaError::AlreadyInitialized.into());
                }
                Ok(this)
            }
        }
    };
}

load!(Pool, POOL_LEN, POOL_VERSION, |pool: &Pool, len: usize| {
    len == POOL_LEN + inventory_space(pool.inventory_version())
});
load!(Item, ITEM_LEN, ITEM_VERSION, |_: &Item, len: usize| len
    == ITEM_LEN);
load!(Pull, PULL_LEN, PULL_VERSION, |_: &Pull, len: usize| len
    == PULL_LEN);
