use crate::assets::Transfer;
use crate::constants::POOL_RETIRED;
use crate::errors::GachaError;
use crate::events::{ReclaimEvent, SetStatusEvent};
use crate::helpers::close;
use crate::inventory::Inventory;
use crate::state::{Item, Load, Pool};
use core::mem::size_of;
use pinocchio::instruction::Signer;
use pinocchio::log::sol_log;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Set Status
///
/// Authority controls admissions. Retirement is terminal; buyer exits stay open.
///
/// Accounts:
///
/// 1. authority:           [signer]
/// 2. pool:                [mut]
/// 3. event_authority:
/// 4. program:             [executable]    this program, for the event CPI
///
/// Parameters:
/// 1. status: u8,                  // 0 paused, 1 active, 2 retired
///
/// Account Checks:
/// - Authority: signer and equal to pool.authority
/// - Pool: writable, deserialized; leaving retirement is checked in process
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Instruction Checks:
/// - Status: a known value
///
/// Event Data:
/// - discriminator: u8, (255u8, 3u8)
/// - pool: Pubkey,
/// - status: u8,
pub struct SetStatusAccounts<'a> {
    pub authority: &'a AccountInfo,
    pub pool: &'a AccountInfo,
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for SetStatusAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, pool, event_authority, program] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !pool.is_writable() {
            return Err(GachaError::NotMutable.into());
        }

        let pool_data = Pool::load(pool)?;
        if pool_data.authority().ne(authority.key()) {
            return Err(GachaError::InvalidAuthority.into());
        }

        // Return the accounts
        Ok(Self {
            authority,
            pool,
            event_authority,
            program,
        })
    }
}

pub struct SetStatusInstructionData {
    pub status: u8,
}

impl<'a> TryFrom<&'a [u8]> for SetStatusInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len().ne(&size_of::<u8>()) {
            return Err(ProgramError::InvalidInstructionData);
        }

        let status = data[0];

        // Instruction Checks
        if status > POOL_RETIRED {
            return Err(ProgramError::InvalidInstructionData);
        }

        Ok(Self { status })
    }
}

pub struct SetStatus<'a> {
    pub accounts: SetStatusAccounts<'a>,
    pub instruction_data: SetStatusInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for SetStatus<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        sol_log("Set Status");

        let accounts = SetStatusAccounts::try_from(accounts)?;
        let instruction_data = SetStatusInstructionData::try_from(data)?;

        // Return the initialized struct
        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> SetStatus<'a> {
    pub const DISCRIMINATOR: &'a u8 = &3;

    pub fn process(&mut self) -> ProgramResult {
        // Retirement is terminal
        let pool = Pool::load_mut(self.accounts.pool)?;
        if pool.status().eq(&POOL_RETIRED) && self.instruction_data.status.ne(&POOL_RETIRED) {
            return Err(GachaError::InvalidPoolStatus.into());
        }

        // Set the status
        pool.set_status(self.instruction_data.status);

        // Log the Set Status Event
        SetStatusEvent {
            pool: self.accounts.pool.key(),
            status: self.instruction_data.status,
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}

/// # Reclaim
///
/// Return one unsold asset and its Item rent to the authority after retirement.
/// The Pool stays alive so awarded prizes remain deliverable.
///
/// > Remove the position from the availability index
/// > Transfer the asset from the pool PDA to the authority
/// > Close the Item account to the authority
///
/// Accounts:
///
/// 1. authority:           [signer, mut]   receives the asset and Item rent
/// 2. pool:                [mut]
/// 3. item:                [mut]
/// 4. asset:               [mut]
/// 5. collection:                          the asset's collection, or the Core program
/// 6. core_program:        [executable]
/// 7. system_program:      [executable]
/// 8. event_authority:
/// 9. program:             [executable]    this program, for the event CPI
///
/// Parameters: None
///
/// Account Checks:
/// - Authority: signer, writable and equal to pool.authority
/// - Pool: writable, deserialized, retired, no pending purchases
/// - Item: writable, deserialized, belongs to this pool and asset, still available
/// - Asset, Collection, CoreProgram, SystemProgram: no need to check since Core
///   validates the asset, its owner and the collection on transfer
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Event Data:
/// - discriminator: u8, (255u8, 4u8)
/// - pool: Pubkey,
/// - asset: Pubkey,
/// - position: u32,
pub struct ReclaimAccounts<'a> {
    pub authority: &'a AccountInfo,
    pub pool: &'a AccountInfo,
    pub item: &'a AccountInfo,
    pub asset: &'a AccountInfo,
    pub collection: &'a AccountInfo,
    pub core_program: &'a AccountInfo,
    pub system_program: &'a AccountInfo,
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for ReclaimAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, pool, item, asset, collection, core_program, system_program, event_authority, program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !authority.is_writable() || !pool.is_writable() || !item.is_writable() {
            return Err(GachaError::NotMutable.into());
        }

        let pool_data = Pool::load(pool)?;
        if pool_data.authority().ne(authority.key()) {
            return Err(GachaError::InvalidAuthority.into());
        }
        if pool_data.status().ne(&POOL_RETIRED) {
            return Err(GachaError::InvalidPoolStatus.into());
        }
        if pool_data.pending_draws().ne(&0) {
            return Err(GachaError::PendingPurchases.into());
        }

        let item_data = Item::load(item)?;
        if item_data.pool().ne(pool.key())
            || item_data.asset().ne(asset.key())
            || item_data.tier() >= pool_data.tier_count()
            || item_data.position() >= pool_data.inventory_version()
            || pool_data.tiers()[item_data.tier() as usize].remaining() == 0
        {
            return Err(GachaError::InvalidItem.into());
        }

        // Return the accounts
        Ok(Self {
            authority,
            pool,
            item,
            asset,
            collection,
            core_program,
            system_program,
            event_authority,
            program,
        })
    }
}

pub struct Reclaim<'a> {
    pub accounts: ReclaimAccounts<'a>,
}

impl<'a> TryFrom<&'a [AccountInfo]> for Reclaim<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        sol_log("Reclaim");

        let accounts = ReclaimAccounts::try_from(accounts)?;

        // Return the initialized struct
        Ok(Self { accounts })
    }
}

impl<'a> Reclaim<'a> {
    pub const DISCRIMINATOR: &'a u8 = &4;

    pub fn process(&mut self) -> ProgramResult {
        let item = Item::load(self.accounts.item)?;
        let (tier, position) = (item.tier(), item.position());

        // Remove the position from the availability index
        let pool = Pool::load_mut(self.accounts.pool)?;
        let mut inventory = Inventory::load(self.accounts.pool)?;
        inventory.remove(position, tier)?;
        let selected = &mut pool.tiers_mut()[tier as usize];
        selected.set_remaining(selected.remaining() - 1);

        // Return the asset to the authority
        let seeds = pool.signer_seeds();
        let seeds = seeds.as_seeds();
        Transfer {
            payer: self.accounts.authority,
            authority: self.accounts.pool,
            new_owner: self.accounts.authority,
            asset: self.accounts.asset,
            collection: self.accounts.collection,
            core_program: self.accounts.core_program,
            system_program: self.accounts.system_program,
        }
        .invoke_signed(&[Signer::from(&seeds)])?;

        // Close the Item to the authority
        close(self.accounts.item, self.accounts.authority)?;

        // Log the Reclaim Event
        ReclaimEvent {
            pool: self.accounts.pool.key(),
            asset: self.accounts.asset.key(),
            position,
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
