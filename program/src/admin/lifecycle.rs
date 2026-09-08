use crate::{
    assets::Transfer,
    constants::*,
    errors::GachaError,
    helpers::close,
    inventory::Inventory,
    state::{Item, Pool},
};
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    ProgramResult,
};

/// Authority controls admissions. Retirement is terminal; buyer exits stay open.
/// Accounts: authority signer, writable pool. Data: status (u8).
pub(crate) struct SetStatus<'a> {
    pool: &'a AccountInfo,
    status: u8,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for SetStatus<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        let [authority, pool] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        let [status] = data else {
            return Err(ProgramError::InvalidInstructionData);
        };
        if *status > POOL_RETIRED {
            return Err(ProgramError::InvalidInstructionData);
        }
        check_authority(authority, pool)?;
        let header = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
        if header.status() == POOL_RETIRED && *status != POOL_RETIRED {
            return Err(GachaError::InvalidPoolStatus.into());
        }
        Ok(Self {
            pool,
            status: *status,
        })
    }
}

impl SetStatus<'_> {
    pub(crate) const DISCRIMINATOR: u8 = 3;

    pub(crate) fn process(self) -> ProgramResult {
        let pool = unsafe { Pool::from_bytes_unchecked_mut(self.pool.borrow_mut_data_unchecked()) };
        pool.set_status(self.status);
        Ok(())
    }
}

/// Return an unsold asset and Item rent to the authority; Pool stays alive.
/// Accounts match deposit: authority, pool, item, asset, collection (or Core
/// sentinel), Core program, system program. No instruction data.
pub(crate) struct Reclaim<'a> {
    authority: &'a AccountInfo,
    pool: &'a AccountInfo,
    item: &'a AccountInfo,
    asset: &'a AccountInfo,
    collection: &'a AccountInfo,
    core_program: &'a AccountInfo,
    system_program: &'a AccountInfo,
    position: u32,
    tier: u8,
    remaining: u32,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Reclaim<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        let [authority, pool, item, asset, collection, core_program, system_program] = accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        if !data.is_empty() {
            return Err(ProgramError::InvalidInstructionData);
        }
        check_authority(authority, pool)?;
        if !authority.is_writable() || !item.is_writable() || !asset.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        crate::state::check_item(item)?;
        let pool_state = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
        if pool_state.status() != POOL_RETIRED {
            return Err(GachaError::InvalidPoolStatus.into());
        }
        if pool_state.pending_draws() != 0 {
            return Err(GachaError::PendingPurchases.into());
        }
        let item_state = unsafe { Item::from_bytes_unchecked(item.borrow_data_unchecked()) };
        if item_state.pool() != pool.key()
            || item_state.asset() != asset.key()
            || item_state.tier() >= pool_state.tier_count()
            || item_state.position() >= pool_state.inventory_version()
        {
            return Err(GachaError::InvalidItem.into());
        }
        let position = item_state.position();
        let tier = item_state.tier();
        let remaining = pool_state.tiers()[tier as usize]
            .remaining()
            .checked_sub(1)
            .ok_or(GachaError::InvalidItem)?;
        Ok(Self {
            authority,
            pool,
            item,
            asset,
            collection,
            core_program,
            system_program,
            position,
            tier,
            remaining,
        })
    }
}

impl Reclaim<'_> {
    pub(crate) const DISCRIMINATOR: u8 = 4;

    pub(crate) fn process(self) -> ProgramResult {
        let data = unsafe { self.pool.borrow_mut_data_unchecked() };
        let (header, index) = data.split_at_mut(POOL_LEN);
        let pool = unsafe { Pool::from_bytes_unchecked_mut(header) };
        Inventory::new(index).remove(self.position, self.tier)?;
        pool.tiers_mut()[self.tier as usize].set_remaining(self.remaining);
        let id = pool.id().to_le_bytes();
        let bump = [pool.bump()];
        let seeds = [
            Seed::from(POOL_SEED),
            Seed::from(pool.authority()),
            Seed::from(&id),
            Seed::from(&bump),
        ];
        Transfer {
            payer: self.authority,
            authority: self.pool,
            new_owner: self.authority,
            asset: self.asset,
            collection: self.collection,
            core_program: self.core_program,
            system_program: self.system_program,
        }
        .invoke_signed(&[Signer::from(&seeds)])?;
        close(self.item, self.authority)
    }
}

fn check_authority(authority: &AccountInfo, pool: &AccountInfo) -> ProgramResult {
    if !authority.is_signer() {
        return Err(GachaError::NotSigner.into());
    }
    if !pool.is_writable() {
        return Err(GachaError::NotMutable.into());
    }
    crate::state::check_pool(pool)?;
    let header = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
    if header.authority() != authority.key() {
        return Err(GachaError::InvalidAuthority.into());
    }
    Ok(())
}
