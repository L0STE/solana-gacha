//! Program-owned account checks before taking the shared protocol views.

use crate::{constants::*, errors::GachaError};
pub(crate) use gacha_core::state::{Item, Pool, Pull};
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

macro_rules! account_check {
    ($check:ident, $name:ident, $len:expr, $version:expr, $valid_len:expr) => {
        pub(crate) fn $check(account: &AccountInfo) -> Result<(), ProgramError> {
            if unlikely(!account.is_owned_by(&crate::ID)) {
                return Err(GachaError::InvalidAccountOwner.into());
            }
            if unlikely(account.data_len() < $len) {
                return Err(GachaError::InvalidAccountLength.into());
            }
            let this = unsafe { $name::from_bytes_unchecked(account.borrow_data_unchecked()) };
            if unlikely(this.version() != $version) {
                return Err(GachaError::InvalidVersion.into());
            }
            if unlikely(!($valid_len)(this, account.data_len())) {
                return Err(GachaError::InvalidAccountLength.into());
            }
            Ok(())
        }
    };
}

account_check!(
    check_pool,
    Pool,
    POOL_LEN,
    POOL_VERSION,
    |pool: &Pool, len: usize| { len == POOL_LEN + inventory_space(pool.inventory_version()) }
);
account_check!(
    check_item,
    Item,
    ITEM_LEN,
    ITEM_VERSION,
    |_: &Item, len: usize| len == ITEM_LEN
);
account_check!(
    check_pull,
    Pull,
    PULL_LEN,
    PULL_VERSION,
    |_: &Pull, len: usize| len == PULL_LEN
);
