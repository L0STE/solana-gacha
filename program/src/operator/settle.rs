use crate::constants::{
    MAX_COUNT, OUTCOMES_OFFSET, OUTCOME_LEN, POOL_LEN, STATUS_PENDING, STATUS_SETTLED,
};
use crate::errors::GachaError;
use crate::events::SettleEvent;
use crate::helpers::sha256;
use crate::inventory::{draw, Inventory};
use crate::state::{Item, Load, Pool, Pull};
use core::mem::size_of;
use pinocchio::log::sol_log;
use pinocchio::sysvars::{clock::Clock, Sysvar};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};
use solana_ecvrf::{Proof, PublicKey};

/// # Settle
///
/// Reveal a pull. The hot path: the operator runs it once per pull, in
/// index order, right after each buy lands. Anyone with the proof can relay it.
///
/// > Verify the proof against the operator key and the pull's alpha
/// > For each draw, pick a tier by weight and an item by rank within the pinned prefix
/// > Record the awards, close the Item accounts, advance the queue
///
/// Accounts:
///
/// 1. operator:            [mut]           receives closed item rent; no signature needed
/// 2. pool:                [mut]
/// 3. pull:                [mut]
/// 4. items:               [mut]           count: the drawn item for each outcome
/// 5. event_authority:
/// 6. program:             [executable]    this program, for the event CPI
///
/// Parameters:
/// 1. proof: [u8; 80],             // Gamma ‖ c ‖ s
///
/// Note: The operator already knows the outcome, so it passes exactly the drawn
/// Item accounts. Which item each draw selects is only known after the draw, so the
/// match is checked in the draw loop rather than deriving PDAs on the hot path.
///
/// Account Checks:
/// - Operator: writable and equal to pool.operator (that key is the VRF key)
/// - Pool: writable, deserialized
/// - Pull: writable, deserialized, belongs to the pool, pending, head of the queue,
///   before its deadline
/// - Items: one writable, deserialized Item per draw
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Instruction Checks:
/// - Proof: verifies against pool.operator and alpha; verification yields the output
///   the draw loop consumes, so it runs in process
///
/// Event Data:
/// - discriminator: u8, (255u8, 20u8)
/// - pool: Pubkey,
/// - pull: Pubkey,
/// - alpha: [u8; 32],
/// - proof: [u8; 80],
/// - beta: [u8; 64],
/// - outcomes: count × (tier: u8, asset: Pubkey),
pub struct SettleAccounts<'a> {
    pub operator: &'a AccountInfo,
    pub pool: &'a AccountInfo,
    pub pull: &'a AccountInfo,
    pub items: &'a [AccountInfo],
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for SettleAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [operator, pool, pull, items @ .., event_authority, program] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !operator.is_writable() || !pool.is_writable() || !pull.is_writable() {
            return Err(GachaError::NotMutable.into());
        }

        let pool_data = Pool::load(pool)?;
        let pull_data = Pull::load(pull)?;
        if pool_data.operator().ne(operator.key()) {
            return Err(GachaError::InvalidOperator.into());
        }
        if pull_data.pool().ne(pool.key()) {
            return Err(GachaError::PoolMismatch.into());
        }
        if pull_data.status().ne(&STATUS_PENDING) {
            return Err(GachaError::InvalidPullStatus.into());
        }
        if pull_data.index().ne(&pool_data.next_settle()) {
            return Err(GachaError::NotNextInQueue.into());
        }
        if Clock::get()?.slot > pull_data.deadline_slot() {
            return Err(GachaError::DeadlinePassed.into());
        }
        if items.len().ne(&(pull_data.count() as usize)) {
            return Err(GachaError::InvalidItemCount.into());
        }
        for item in items {
            if !item.is_writable() {
                return Err(GachaError::NotMutable.into());
            }
            Item::load(item)?;
        }

        // Return the accounts
        Ok(Self {
            operator,
            pool,
            pull,
            items,
            event_authority,
            program,
        })
    }
}

pub struct SettleInstructionData {
    pub proof: Proof,
}

impl<'a> TryFrom<&'a [u8]> for SettleInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len().ne(&size_of::<[u8; 80]>()) {
            return Err(ProgramError::InvalidInstructionData);
        }

        let proof = Proof(data.try_into().unwrap());

        Ok(Self { proof })
    }
}

pub struct Settle<'a> {
    pub accounts: SettleAccounts<'a>,
    pub instruction_data: SettleInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Settle<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        sol_log("Settle");

        let accounts = SettleAccounts::try_from(accounts)?;
        let instruction_data = SettleInstructionData::try_from(data)?;

        // Return the initialized struct
        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Settle<'a> {
    pub const DISCRIMINATOR: &'a u8 = &20;

    pub fn process(&mut self) -> ProgramResult {
        let pool = Pool::load_mut(self.accounts.pool)?;
        let inventory = Inventory::load(self.accounts.pool)?;
        let pull = Pull::load_mut(self.accounts.pull)?;
        let count = self.accounts.items.len();

        // Only verified output reaches the draw loop
        let operator = PublicKey(*pool.operator());
        let alpha = sha256(&[self.accounts.pull.key(), pull.client_seed()]);
        let beta = self
            .instruction_data
            .proof
            .verify(&operator, &alpha)
            .map_err(|_| GachaError::InvalidProof)?;

        // Draw within the pinned prefix and record the awards
        draw(
            pool,
            inventory,
            pull,
            self.accounts.pool.key(),
            self.accounts.items,
            self.accounts.operator,
            &beta,
        )?;

        // Release the reservation and advance the queue
        pool.set_pending_draws(pool.pending_draws() - count as u64);
        pool.set_next_settle(pull.index() + 1);
        pull.set_status(STATUS_SETTLED);

        // Log the Settle Event
        SettleEvent {
            pool: self.accounts.pool.key(),
            pull: self.accounts.pull.key(),
            alpha: &alpha,
            proof: &self.instruction_data.proof.0,
            beta: &beta,
            outcomes: pull.outcome_bytes(count),
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
