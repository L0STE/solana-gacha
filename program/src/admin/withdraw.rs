use crate::errors::GachaError;
use crate::events::WithdrawEvent;
use crate::helpers::{pay, spendable};
use crate::state::{Load, Pool};
use core::mem::size_of;
use pinocchio::log::sol_log;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Withdraw
///
/// Take unreserved funds out of the vault. Every pending draw retains its
/// full payment plus timeout penalty. With no pending draws, all funds are free.
///
/// Accounts:
///
/// 1. authority:           [signer]
/// 2. pool:
/// 3. vault:               [mut]
/// 4. destination:         [mut]
/// 5. token_program:       [executable]
/// 6. event_authority:
/// 7. program:             [executable]    this program, for the event CPI
///
/// Parameters:
/// 1. amount: u64,
///
/// Account Checks:
/// - Authority: signer and equal to pool.authority
/// - Pool: deserialized
/// - Vault: equal to pool.vault
/// - Destination: no need to check since the transfer fails on a wrong mint
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Instruction Checks:
/// - Amount: at most the vault balance minus every pending refund, checked in process
///
/// Event Data:
/// - discriminator: u8, (255u8, 2u8)
/// - pool: Pubkey,
/// - amount: u64,
pub struct WithdrawAccounts<'a> {
    pub authority: &'a AccountInfo,
    pub pool: &'a AccountInfo,
    pub vault: &'a AccountInfo,
    pub destination: &'a AccountInfo,
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for WithdrawAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, pool, vault, destination, _token_program, event_authority, program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }

        let pool_data = Pool::load(pool)?;
        if pool_data.authority().ne(authority.key()) {
            return Err(GachaError::InvalidAuthority.into());
        }
        if pool_data.vault().ne(vault.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        // Return the accounts
        Ok(Self {
            authority,
            pool,
            vault,
            destination,
            event_authority,
            program,
        })
    }
}

pub struct WithdrawInstructionData {
    pub amount: u64,
}

impl<'a> TryFrom<&'a [u8]> for WithdrawInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len().ne(&size_of::<u64>()) {
            return Err(ProgramError::InvalidInstructionData);
        }

        let amount = u64::from_le_bytes(data[0..8].try_into().unwrap());

        Ok(Self { amount })
    }
}

pub struct Withdraw<'a> {
    pub accounts: WithdrawAccounts<'a>,
    pub instruction_data: WithdrawInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Withdraw<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        sol_log("Withdraw");

        let accounts = WithdrawAccounts::try_from(accounts)?;
        let instruction_data = WithdrawInstructionData::try_from(data)?;

        // Return the initialized struct
        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Withdraw<'a> {
    pub const DISCRIMINATOR: &'a u8 = &2;

    pub fn process(&mut self) -> ProgramResult {
        // Only surplus above every pending refund is free
        let pool = Pool::load(self.accounts.pool)?;
        if self.instruction_data.amount > spendable(pool, self.accounts.vault)? {
            return Err(GachaError::InsufficientBalance.into());
        }

        // Pay the destination from the vault
        pay(
            self.accounts.pool,
            self.accounts.vault,
            self.accounts.destination,
            self.instruction_data.amount,
        )?;

        // Log the Withdraw Event
        WithdrawEvent {
            pool: self.accounts.pool.key(),
            amount: self.instruction_data.amount,
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
