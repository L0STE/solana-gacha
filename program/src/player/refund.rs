use crate::constants::STATUS_PENDING;
use crate::errors::GachaError;
use crate::events::RefundEvent;
use crate::helpers::{close, TokenAccount};
use crate::state::{Load, Pool, Pull};
use pinocchio::instruction::Signer;
use pinocchio::log::sol_log;
use pinocchio::sysvars::{clock::Clock, Sysvar};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};
use pinocchio_token::instructions::Transfer;

/// # Refund
///
/// The operator went dark: return the price and pay the bond to the buyer.
/// Only the head of the settle queue can be refunded, so the queue stays
/// contiguous; a stalled operator is refunded pull by pull, in order.
/// Permissionless, funds always go to the recorded buyer.
///
/// > Release the reserved draws and advance the queue
/// > Transfer payment plus penalty from the vault to the buyer
/// > Close the Pull to the buyer
///
/// Accounts:
///
/// 1. pool:                [mut]
/// 2. pull:                [mut]
/// 3. buyer:               [mut]           receives the pull rent
/// 4. vault:               [mut]
/// 5. buyer_ata:           [mut]
/// 6. token_program:       [executable]
/// 7. event_authority:
/// 8. program:             [executable]    this program, for the event CPI
///
/// Parameters: None
///
/// Account Checks:
/// - Pool: writable, deserialized
/// - Pull: writable, deserialized, belongs to the pool, pending, head of the queue,
///   past its deadline
/// - Buyer: writable and equal to pull.buyer
/// - Vault: equal to pool.vault
/// - BuyerAta: owned by the buyer; no need to check the mint since the transfer fails
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Event Data:
/// - discriminator: u8, (255u8, 11u8)
/// - pool: Pubkey,
/// - pull: Pubkey,
/// - amount: u64,
pub struct RefundAccounts<'a> {
    pub pool: &'a AccountInfo,
    pub pull: &'a AccountInfo,
    pub buyer: &'a AccountInfo,
    pub vault: &'a AccountInfo,
    pub buyer_ata: &'a AccountInfo,
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for RefundAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [pool, pull, buyer, vault, buyer_ata, _token_program, event_authority, program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !pool.is_writable() || !pull.is_writable() || !buyer.is_writable() {
            return Err(GachaError::NotMutable.into());
        }

        let pool_data = Pool::load(pool)?;
        let pull_data = Pull::load(pull)?;
        if pull_data.pool().ne(pool.key()) {
            return Err(GachaError::PoolMismatch.into());
        }
        if pull_data.status().ne(&STATUS_PENDING) {
            return Err(GachaError::InvalidPullStatus.into());
        }
        if pull_data.index().ne(&pool_data.next_settle()) {
            return Err(GachaError::NotNextInQueue.into());
        }
        if Clock::get()?.slot <= pull_data.deadline_slot() {
            return Err(GachaError::DeadlineNotReached.into());
        }
        if pull_data.buyer().ne(buyer.key()) {
            return Err(GachaError::InvalidBuyer.into());
        }
        if pool_data.vault().ne(vault.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }
        let buyer_ata_data = TokenAccount::load(buyer_ata)?;
        if buyer_ata_data.owner.ne(pull_data.buyer()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        // Return the accounts
        Ok(Self {
            pool,
            pull,
            buyer,
            vault,
            buyer_ata,
            event_authority,
            program,
        })
    }
}

pub struct Refund<'a> {
    pub accounts: RefundAccounts<'a>,
}

impl<'a> TryFrom<&'a [AccountInfo]> for Refund<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        sol_log("Refund");

        let accounts = RefundAccounts::try_from(accounts)?;

        // Return the initialized struct
        Ok(Self { accounts })
    }
}

impl<'a> Refund<'a> {
    pub const DISCRIMINATOR: &'a u8 = &11;

    pub fn process(&mut self) -> ProgramResult {
        // Release the reservation and advance the queue
        let pool = Pool::load_mut(self.accounts.pool)?;
        let pull = Pull::load(self.accounts.pull)?;
        let count = pull.count() as u64;
        let amount = pool.refund_amount(count)?;
        pool.set_pending_draws(pool.pending_draws() - count);
        pool.set_next_settle(pull.index() + 1);

        // Pay the buyer the price plus the penalty
        let seeds = pool.signer_seeds();
        let seeds = seeds.as_seeds();
        Transfer {
            from: self.accounts.vault,
            to: self.accounts.buyer_ata,
            authority: self.accounts.pool,
            amount,
        }
        .invoke_signed(&[Signer::from(&seeds)])?;

        // Close the Pull to the buyer
        close(self.accounts.pull, self.accounts.buyer)?;

        // Log the Refund Event
        RefundEvent {
            pool: self.accounts.pool.key(),
            pull: self.accounts.pull.key(),
            amount,
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
