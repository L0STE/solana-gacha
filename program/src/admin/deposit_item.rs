use crate::{assets::Transfer, errors::GachaError, state::Pool};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// Deposit one Core asset at a fresh position in the selected tier.
/// Accounts: authority (payer/signer), pool, item, asset, collection (or Core
/// sentinel), Core program, system program. Pool, item and asset are writable.
pub(crate) struct DepositItem<'a> {
    authority: &'a AccountInfo,
    pool: &'a AccountInfo,
    item: &'a AccountInfo,
    asset: &'a AccountInfo,
    collection: &'a AccountInfo,
    core_program: &'a AccountInfo,
    system_program: &'a AccountInfo,
    tier: u8,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for DepositItem<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        let [authority, pool, item, asset, collection, core_program, system_program] = accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        let [tier] = data else {
            return Err(ProgramError::InvalidInstructionData);
        };
        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !authority.is_writable()
            || !pool.is_writable()
            || !item.is_writable()
            || !asset.is_writable()
        {
            return Err(GachaError::NotMutable.into());
        }
        crate::state::check_pool(pool)?;
        let header = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
        if header.authority() != authority.key() {
            return Err(GachaError::InvalidAuthority.into());
        }
        Ok(Self {
            authority,
            pool,
            item,
            asset,
            collection,
            core_program,
            system_program,
            tier: *tier,
        })
    }
}

impl DepositItem<'_> {
    pub(crate) const DISCRIMINATOR: u8 = 1;

    pub(crate) fn process(self) -> ProgramResult {
        crate::inventory::restock(
            self.authority,
            self.pool,
            self.item,
            self.asset.key(),
            self.tier,
        )?;
        Transfer {
            payer: self.authority,
            authority: self.authority,
            new_owner: self.pool,
            asset: self.asset,
            collection: self.collection,
            core_program: self.core_program,
            system_program: self.system_program,
        }
        .invoke_signed(&[])
    }
}
