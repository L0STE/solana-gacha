use crate::constants::*;
use crate::errors::GachaError;
use crate::helpers::token_account;
use crate::state::Pool;
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    ProgramResult,
};
use pinocchio_token::instructions::Transfer;

/// # Withdraw
///
/// Take unreserved funds out of the vault. Every pending draw retains its
/// full payment plus timeout penalty. With no pending draws, all funds are free.
///
/// Accounts:
///
/// 1. authority:      [signer]
/// 2. pool:
/// 3. vault:          [mut]
/// 4. destination:    [mut]
/// 5. token_program:  [executable]
///
/// Parameters:
/// 1. amount: u64,
///
/// Account Checks:
/// - Authority: signer, equals pool.authority
/// - Pool: Pool::check
/// - Vault: equals pool.vault
/// - Destination: the transfer fails on a wrong mint
///
/// Instruction Checks:
/// - amount ≤ free balance
struct WithdrawAccounts<'a> {
    pool: &'a AccountInfo,
    vault: &'a AccountInfo,
    destination: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for WithdrawAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, pool, vault, destination, _token_program] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        Pool::check(pool)?;
        let header = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
        if header.authority().ne(authority.key()) {
            return Err(GachaError::InvalidAuthority.into());
        }
        if header.vault().ne(vault.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        Ok(Self {
            pool,
            vault,
            destination,
        })
    }
}

pub(crate) struct Withdraw<'a> {
    accounts: WithdrawAccounts<'a>,
    amount: u64,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Withdraw<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("Withdraw");

        if data.len() != 8 {
            return Err(ProgramError::InvalidInstructionData);
        }
        Ok(Self {
            accounts: WithdrawAccounts::try_from(accounts)?,
            amount: u64::from_le_bytes(data.try_into().unwrap()),
        })
    }
}

impl<'a> Withdraw<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 2;

    pub(crate) fn process(self) -> ProgramResult {
        let accounts = &self.accounts;
        let pool = unsafe { Pool::from_bytes_unchecked(accounts.pool.borrow_data_unchecked()) };
        let (_, _, vault_balance) = token_account(accounts.vault)?;
        let free_balance = vault_balance
            .checked_sub(pool.refund_amount(pool.pending_draws())?)
            .ok_or(GachaError::InsufficientBalance)?;
        if self.amount > free_balance {
            return Err(GachaError::InsufficientBalance.into());
        }

        let id = pool.id().to_le_bytes();
        let bump = [pool.bump()];
        let seeds = [
            Seed::from(POOL_SEED),
            Seed::from(pool.authority()),
            Seed::from(&id),
            Seed::from(&bump),
        ];
        Transfer {
            from: accounts.vault,
            to: accounts.destination,
            authority: accounts.pool,
            amount: self.amount,
        }
        .invoke_signed(&[Signer::from(&seeds)])
    }
}
