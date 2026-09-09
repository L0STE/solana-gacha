use crate::assets::Transfer;
use crate::constants::POOL_RETIRED;
use crate::errors::GachaError;
use crate::events::DepositItemEvent;
use crate::inventory::{check_restock, restock};
use crate::state::{Load, Pool};
use core::mem::size_of;
use pinocchio::log::sol_log;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Deposit Item
///
/// Stock one Core asset at a fresh, never-reused inventory position.
///
/// > Create the Item account and append the position to the availability index
/// > Transfer the asset from the authority to the pool PDA
///
/// Accounts:
///
/// 1. authority:           [signer, mut]   pays Item and index rent
/// 2. pool:                [mut]
/// 3. item:                [mut]           PDA [ITEM_SEED, pool, tier, position]
/// 4. asset:               [mut]           Core AssetV1 owned by the authority
/// 5. collection:                          the asset's collection, or the Core program
/// 6. core_program:        [executable]
/// 7. system_program:      [executable]
/// 8. event_authority:
/// 9. program:             [executable]    this program, for the event CPI
///
/// Parameters:
/// 1. tier: u8,
/// 2. bump: u8,                    // item PDA bump for the pool's next position
///
/// Account Checks:
/// - Authority: signer and equal to pool.authority
/// - Pool: writable, deserialized, not retired
/// - Item: writable; the PDA and emptiness checks need the tier, so they run in process
/// - Asset, Collection, CoreProgram, SystemProgram: no need to check since Core
///   validates the asset, its owner's signature and the collection on transfer
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Instruction Checks:
/// - Tier: needs pool.tier_count, so it runs in process
///
/// Event Data:
/// - discriminator: u8, (255u8, 1u8)
/// - pool: Pubkey,
/// - asset: Pubkey,
/// - tier: u8,
/// - position: u32,
pub struct DepositItemAccounts<'a> {
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

impl<'a> TryFrom<&'a [AccountInfo]> for DepositItemAccounts<'a> {
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
        if !pool.is_writable() || !item.is_writable() {
            return Err(GachaError::NotMutable.into());
        }

        let pool_data = Pool::load(pool)?;
        if pool_data.authority().ne(authority.key()) {
            return Err(GachaError::InvalidAuthority.into());
        }
        if pool_data.status().eq(&POOL_RETIRED) {
            return Err(GachaError::InvalidPoolStatus.into());
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

pub struct DepositItemInstructionData {
    pub tier: u8,
    pub bump: u8,
}

impl<'a> TryFrom<&'a [u8]> for DepositItemInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len().ne(&(size_of::<u8>() * 2)) {
            return Err(ProgramError::InvalidInstructionData);
        }

        let tier = data[0];
        let bump = data[1];

        Ok(Self { tier, bump })
    }
}

pub struct DepositItem<'a> {
    pub accounts: DepositItemAccounts<'a>,
    pub instruction_data: DepositItemInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for DepositItem<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        sol_log("Deposit Item");

        let accounts = DepositItemAccounts::try_from(accounts)?;
        let instruction_data = DepositItemInstructionData::try_from(data)?;

        // Return the initialized struct
        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> DepositItem<'a> {
    pub const DISCRIMINATOR: &'a u8 = &1;

    pub fn process(&mut self) -> ProgramResult {
        // The tier must exist and the item must be the empty PDA for the next position
        let pool = Pool::load(self.accounts.pool)?;
        let position = check_restock(
            pool,
            self.accounts.pool.key(),
            self.accounts.item,
            self.instruction_data.tier,
            self.instruction_data.bump,
        )?;

        // Create the Item and append its position to the availability index
        restock(
            self.accounts.authority,
            self.accounts.pool,
            self.accounts.item,
            self.accounts.asset.key(),
            self.instruction_data.tier,
            self.instruction_data.bump,
        )?;

        // Take custody of the asset
        Transfer {
            payer: self.accounts.authority,
            authority: self.accounts.authority,
            new_owner: self.accounts.pool,
            asset: self.accounts.asset,
            collection: self.accounts.collection,
            core_program: self.accounts.core_program,
            system_program: self.accounts.system_program,
        }
        .invoke_signed(&[])?;

        // Log the Deposit Item Event
        DepositItemEvent {
            pool: self.accounts.pool.key(),
            asset: self.accounts.asset.key(),
            tier: self.instruction_data.tier,
            position,
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
