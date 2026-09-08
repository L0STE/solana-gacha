use crate::constants::*;
use crate::errors::GachaError;
use crate::helpers::{close, token_account};
use crate::state::{Pool, Pull};
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};
use pinocchio_token::instructions::Transfer;

/// # Refund
///
/// The operator went dark: return the price and pay the bond to the buyer.
/// Only the head of the settle queue can be refunded, so the queue stays
/// contiguous; a stalled operator is refunded pull by pull, in order.
/// Permissionless, funds always go to the recorded buyer.
///
/// Accounts:
///
/// 1. pool:           [mut]
/// 2. pull:           [mut]
/// 3. buyer:          [mut]           receives the pull rent
/// 4. vault:          [mut]
/// 5. buyer_ata:      [mut]
/// 6. token_program:  [executable]
///
/// Parameters: none.
///
/// Account Checks:
/// - Pool, Pull: owner, length, version; pull.pool equals pool
/// - Buyer: equals pull.buyer
/// - Vault: equals pool.vault
/// - Buyer ATA: owned by the buyer; the transfer fails on a wrong mint
///
/// Instruction Checks:
/// - status pending, index equals next_settle, slot > deadline_slot
pub(crate) struct Refund<'a> {
    pool: &'a AccountInfo,
    pull: &'a AccountInfo,
    buyer: &'a AccountInfo,
    vault: &'a AccountInfo,
    buyer_ata: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for Refund<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("Refund");

        let [pool, pull, buyer, vault, buyer_ata, _token_program] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        if !pool.is_writable() || !pull.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        Pool::check(pool)?;
        Pull::check(pull)?;
        Ok(Self {
            pool,
            pull,
            buyer,
            vault,
            buyer_ata,
        })
    }
}

impl<'a> Refund<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 11;

    pub(crate) fn process(self) -> ProgramResult {
        let pool = unsafe { Pool::from_bytes_unchecked_mut(self.pool.borrow_mut_data_unchecked()) };
        let pull = unsafe { Pull::from_bytes_unchecked(self.pull.borrow_data_unchecked()) };
        if pull.pool().ne(self.pool.key()) || pull.status() != STATUS_PENDING {
            return Err(GachaError::InvalidPullStatus.into());
        }
        if pull.index() != pool.next_settle() {
            return Err(GachaError::NotNextInQueue.into());
        }
        if Clock::get()?.slot <= pull.deadline_slot() {
            return Err(GachaError::DeadlineNotReached.into());
        }
        if pull.buyer().ne(self.buyer.key()) {
            return Err(GachaError::InvalidBuyer.into());
        }
        if pool.vault().ne(self.vault.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }
        let (owner, _, _) = token_account(self.buyer_ata)?;
        if owner.ne(pull.buyer()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        let count = pull.count() as u64;
        let amount = pool.refund_amount(count)?;
        pool.set_pending_draws(
            pool.pending_draws()
                .checked_sub(count)
                .ok_or(ProgramError::ArithmeticOverflow)?,
        );
        pool.set_next_settle(
            pull.index()
                .checked_add(1)
                .ok_or(ProgramError::ArithmeticOverflow)?,
        );

        let id = pool.id().to_le_bytes();
        let bump = [pool.bump()];
        let seeds = [
            Seed::from(POOL_SEED),
            Seed::from(pool.authority()),
            Seed::from(&id),
            Seed::from(&bump),
        ];
        Transfer {
            from: self.vault,
            to: self.buyer_ata,
            authority: self.pool,
            amount,
        }
        .invoke_signed(&[Signer::from(&seeds)])?;

        close(self.pull, self.buyer)
    }
}
