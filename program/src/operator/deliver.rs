use crate::constants::*;
use crate::errors::GachaError;
use crate::helpers::{close, token_account};
use crate::state::{Pool, Pull};
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    ProgramResult,
};
use pinocchio_token::instructions::Transfer;

/// # Deliver
///
/// Hand one settled outcome to its buyer. Permissionless: the operator
/// cranks it right after settle, but anyone may. When the last outcome of a
/// pull is delivered the pull closes and its rent returns to the buyer.
///
/// Accounts:
///
/// 1. pool:
/// 2. pull:           [mut]
/// 3. buyer:          [mut]           receives the pull rent on close
/// 4. pool_ata:       [mut]
/// 5. buyer_ata:      [mut]
/// 6. token_program:  [executable]
///
/// Parameters:
/// 1. outcome: u8,
///
/// Account Checks:
/// - Pool, Pull: owner, length, version; pull.pool equals pool; settled
/// - Buyer: equals pull.buyer
/// - Pool ATA: SPL token account owned by the pool with the outcome's mint
/// - Buyer ATA: SPL token account owned by the buyer with the outcome's mint
///
/// Instruction Checks:
/// - outcome < count and not yet delivered
struct DeliverAccounts<'a> {
    pool: &'a AccountInfo,
    pull: &'a AccountInfo,
    buyer: &'a AccountInfo,
    pool_ata: &'a AccountInfo,
    buyer_ata: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for DeliverAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [pool, pull, buyer, pool_ata, buyer_ata, _token_program] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        if !pull.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        Pool::check(pool)?;
        Pull::check(pull)?;
        Ok(Self {
            pool,
            pull,
            buyer,
            pool_ata,
            buyer_ata,
        })
    }
}

pub(crate) struct Deliver<'a> {
    accounts: DeliverAccounts<'a>,
    outcome: usize,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Deliver<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("Deliver");

        let [outcome] = data else {
            return Err(ProgramError::InvalidInstructionData);
        };
        Ok(Self {
            accounts: DeliverAccounts::try_from(accounts)?,
            outcome: *outcome as usize,
        })
    }
}

impl<'a> Deliver<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 21;

    pub(crate) fn process(self) -> ProgramResult {
        let accounts = &self.accounts;
        let pool = unsafe { Pool::from_bytes_unchecked(accounts.pool.borrow_data_unchecked()) };
        let pull =
            unsafe { Pull::from_bytes_unchecked_mut(accounts.pull.borrow_mut_data_unchecked()) };
        if pull.pool().ne(accounts.pool.key()) || pull.status() != STATUS_SETTLED {
            return Err(GachaError::InvalidPullStatus.into());
        }
        if pull.buyer().ne(accounts.buyer.key()) {
            return Err(GachaError::InvalidBuyer.into());
        }
        if self.outcome >= pull.count() as usize {
            return Err(GachaError::InvalidOutcome.into());
        }
        let (tier, mint) = pull.outcome(self.outcome);
        if tier == DELIVERED {
            return Err(GachaError::InvalidOutcome.into());
        }
        let (owner, source_mint, _) = token_account(accounts.pool_ata)?;
        if owner.ne(accounts.pool.key()) || source_mint.ne(mint) {
            return Err(GachaError::InvalidTokenAddress.into());
        }
        let (owner, destination_mint, _) = token_account(accounts.buyer_ata)?;
        if owner.ne(pull.buyer()) || destination_mint.ne(mint) {
            return Err(GachaError::InvalidTokenAddress.into());
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
            from: accounts.pool_ata,
            to: accounts.buyer_ata,
            authority: accounts.pool,
            amount: 1,
        }
        .invoke_signed(&[Signer::from(&seeds)])?;

        pull.mark_delivered(self.outcome);
        if pull.all_delivered() {
            close(accounts.pull, accounts.buyer)?;
        }
        Ok(())
    }
}
