use crate::constants::*;
use crate::errors::GachaError;
use crate::helpers::{close, log_settle_event, sha256};
use crate::inventory::Inventory;
use crate::state::{Item, Pool, Pull};
use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};
use solana_ecvrf::{Proof, PublicKey};

/// # Settle
///
/// Reveal a pull. The hot path: the operator runs it once per pull, in
/// index order, right after each buy lands. Anyone with the proof can relay it.
///
/// Accounts:
///
/// 1. operator:       [mut]           receives closed item rent; no signature needed
/// 2. pool:           [mut]
/// 3. pull:           [mut]
/// 4. items:          [mut]           count: the drawn item for each outcome
///
/// Parameters:
/// 1. proof: [u8; 80],     // Gamma ‖ c ‖ s
///
/// Account Checks:
/// - Operator: equals pool.operator (that key is the VRF key)
/// - Pool, Pull: owner, length and version; pull.pool equals pool;
///   status pending; pull.index equals pool.next_settle
/// - Items: Item::check; each must carry this pool and the drawn
///   (tier, immutable position) — the operator
///   already knows the outcome, so it passes exactly the right accounts, and
///   a wrong one fails here rather than deriving PDAs on the hot path
///
/// Instruction Checks:
/// - The proof must verify against pool.operator and alpha
///
/// Event Data:
/// - discriminator: u8 (0), pull: Pubkey, alpha: [u8; 32], proof: [u8; 80],
///   beta: [u8; 64], outcomes: count × (tier: u8, mint: Pubkey)
struct SettleAccounts<'a> {
    operator: &'a AccountInfo,
    pool: &'a AccountInfo,
    pull: &'a AccountInfo,
    items: &'a [AccountInfo],
}

impl<'a> TryFrom<&'a [AccountInfo]> for SettleAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [operator, pool, pull, items @ ..] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !pool.is_writable() || !pull.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        Pool::check(pool)?;
        Pull::check(pull)?;

        Ok(Self {
            operator,
            pool,
            pull,
            items,
        })
    }
}

pub(crate) struct Settle<'a> {
    accounts: SettleAccounts<'a>,
    proof: &'a [u8; 80],
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Settle<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("Settle");

        let proof: &[u8; 80] = data
            .try_into()
            .map_err(|_| ProgramError::InvalidInstructionData)?;
        Ok(Self {
            accounts: SettleAccounts::try_from(accounts)?,
            proof,
        })
    }
}

impl<'a> Settle<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 20;

    pub(crate) fn process(self) -> ProgramResult {
        let accounts = &self.accounts;
        let pool_key = accounts.pool.key();
        let data = unsafe { accounts.pool.borrow_mut_data_unchecked() };
        let (header, index) = data.split_at_mut(POOL_LEN);
        let pool = unsafe { Pool::from_bytes_unchecked_mut(header) };
        let mut inventory = Inventory::new(index);
        let pull =
            unsafe { Pull::from_bytes_unchecked_mut(accounts.pull.borrow_mut_data_unchecked()) };

        if pool.operator().ne(accounts.operator.key()) {
            return Err(GachaError::InvalidOperator.into());
        }
        if pull.pool().ne(pool_key) || pull.status() != STATUS_PENDING {
            return Err(GachaError::InvalidPullStatus.into());
        }
        if pull.index() != pool.next_settle() {
            return Err(GachaError::NotNextInQueue.into());
        }
        if Clock::get()?.slot > pull.deadline_slot() {
            return Err(GachaError::DeadlinePassed.into());
        }
        if pull.inventory_version() > pool.inventory_version() {
            return Err(GachaError::InvalidPullStatus.into());
        }
        let count = pull.count() as usize;
        if accounts.items.len() != count {
            return Err(ProgramError::NotEnoughAccountKeys);
        }

        // Verify before using the output to select any prizes.
        let alpha = sha256(&[accounts.pull.key(), pull.client_seed()]);
        let beta = Proof(*self.proof)
            .verify(&PublicKey(*pool.operator()), &alpha)
            .map_err(|_| GachaError::InvalidProof)?;

        // During this instruction, only our own draws reduce the eligible stock.
        let mut available = inventory.counts(pull.inventory_version());
        for i in 0..count {
            let hash = sha256(&[&beta, &[i as u8]]);
            let tier = pool.draw_tier(&hash, &available)?;
            let rank = (u64::from_le_bytes(hash[8..16].try_into().unwrap())
                % available[tier as usize] as u64) as u32;
            let position = inventory.take(tier, rank)?;
            available[tier as usize] -= 1;

            let item_account = &accounts.items[i];
            if !item_account.is_writable() {
                return Err(GachaError::NotMutable.into());
            }
            Item::check(item_account)?;
            let item = unsafe { Item::from_bytes_unchecked(item_account.borrow_data_unchecked()) };
            if item.pool().ne(pool_key) || item.tier() != tier || item.position() != position {
                return Err(GachaError::InvalidItem.into());
            }
            pull.set_outcome(i, tier, item.mint());

            close(item_account, accounts.operator)?;

            let selected_tier = &mut pool.tiers_mut()[tier as usize];
            selected_tier.set_remaining(
                selected_tier
                    .remaining()
                    .checked_sub(1)
                    .ok_or(ProgramError::ArithmeticOverflow)?,
            );
        }

        pool.set_pending_draws(
            pool.pending_draws()
                .checked_sub(count as u64)
                .ok_or(ProgramError::ArithmeticOverflow)?,
        );
        pool.set_next_settle(
            pull.index()
                .checked_add(1)
                .ok_or(ProgramError::ArithmeticOverflow)?,
        );
        pull.set_status(STATUS_SETTLED);

        // Borrow the packed outcomes directly for the event; no copy or allocation.
        let outcomes = unsafe {
            core::slice::from_raw_parts(
                (pull as *const Pull as *const u8).add(OUTCOMES_OFFSET),
                count * OUTCOME_LEN,
            )
        };
        log_settle_event(accounts.pull.key(), &alpha, self.proof, &beta, outcomes);
        Ok(())
    }
}
