use crate::constants::*;
use crate::errors::GachaError;
use crate::helpers::{create_pda, token_account};
use crate::state::{Pool, Pull};
use pinocchio::{
    account_info::AccountInfo,
    instruction::Seed,
    program_error::ProgramError,
    pubkey::find_program_address,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};
use pinocchio_token::instructions::Transfer;

/// # Buy
///
/// Commit to a pull: pay `price × count` into escrow and record the buyer's
/// seed. Randomness is `VRF(operator, SHA-256(pull ‖ seed))`; the operator can
/// compute it once the seed is known. The signed inventory version pins the
/// candidate list before that seed is shared; later deposits cannot enter it.
///
/// Accounts:
///
/// 1. buyer:          [signer, mut]   pays rent and the price
/// 2. pool:           [mut]
/// 3. pull:           [mut]           PDA [PULL_SEED, pool, next_index]
/// 4. buyer_ata:      [mut]
/// 5. vault:          [mut]
/// 6. token_program:  [executable]
/// 7. system_program: [executable]
///
/// Parameters:
/// 1. count: u8,
/// 2. client_seed: [u8; 32],
/// 3. inventory_version: u32, // expected deposit count, read before signing
///
/// Account Checks:
/// - Buyer: signer
/// - Pool: Pool::check; writable
/// - Pull: must be the PDA for (pool, next_index); created here
/// - Vault: equals pool.vault
/// - Buyer ATA: the transfer fails on a wrong mint or owner
///
/// Instruction Checks:
/// - 1 ≤ count ≤ 10; enough unreserved stock and funded timeout penalties
pub(crate) struct Buy<'a> {
    buyer: &'a AccountInfo,
    pool: &'a AccountInfo,
    pull: &'a AccountInfo,
    buyer_ata: &'a AccountInfo,
    vault: &'a AccountInfo,
    count: u8,
    client_seed: &'a [u8; 32],
    index: u64,
    next_index: u64,
    pending_draws: u64,
    amount: u64,
    deadline_slot: u64,
    bump: u8,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Buy<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        #[cfg(feature = "ix-logs")]
        pinocchio::log::sol_log("Buy");

        let [buyer, pool, pull, buyer_ata, vault, _token_program, _system_program] = accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        if !buyer.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !pool.is_writable() || !pull.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        crate::state::check_pool(pool)?;
        let header = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
        if header.vault().ne(vault.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }
        if data.len() != 37 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let count = data[0];
        let client_seed = data[1..33].try_into().unwrap();
        let inventory_version = u32::from_le_bytes(data[33..37].try_into().unwrap());
        if header.status() != POOL_ACTIVE {
            return Err(GachaError::InvalidPoolStatus.into());
        }
        if count == 0 || count as usize > MAX_COUNT {
            return Err(GachaError::InvalidCount.into());
        }
        if inventory_version != header.inventory_version() {
            return Err(GachaError::InventoryChanged.into());
        }
        let index = header.next_index();
        let pending_draws = header
            .pending_draws()
            .checked_add(count as u64)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        // FIFO makes candidate prefixes nested: every older pending draw can
        // consume at most one item in this prefix, so this also reserves enough
        // eligible stock for this purchase even if no further deposits arrive.
        if pending_draws > header.remaining() {
            return Err(GachaError::SoldOut.into());
        }
        let amount = header.payment(count as u64)?;
        let (_, _, vault_balance) = token_account(vault)?;
        let funded_balance = vault_balance
            .checked_add(amount)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        if funded_balance < header.refund_amount(pending_draws)? {
            return Err(GachaError::InsufficientBalance.into());
        }
        let next_index = index
            .checked_add(1)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        let deadline_slot = Clock::get()?
            .slot
            .checked_add(header.deadline_slots())
            .ok_or(ProgramError::ArithmeticOverflow)?;
        let (pull_key, bump) =
            find_program_address(&[PULL_SEED, pool.key(), &index.to_le_bytes()], &crate::ID);
        if pull_key.ne(pull.key()) {
            return Err(GachaError::InvalidAccountOwner.into());
        }

        Ok(Self {
            buyer,
            pool,
            pull,
            buyer_ata,
            vault,
            count,
            client_seed,
            index,
            next_index,
            pending_draws,
            amount,
            deadline_slot,
            bump,
        })
    }
}

impl Buy<'_> {
    pub(crate) const DISCRIMINATOR: u8 = 10;

    pub(crate) fn process(self) -> ProgramResult {
        let index_seed = self.index.to_le_bytes();
        let bump_seed = [self.bump];
        create_pda(
            self.buyer,
            self.pull,
            PULL_LEN,
            &[
                Seed::from(PULL_SEED),
                Seed::from(self.pool.key()),
                Seed::from(&index_seed),
                Seed::from(&bump_seed),
            ],
        )?;
        let pool = unsafe { Pool::from_bytes_unchecked_mut(self.pool.borrow_mut_data_unchecked()) };
        let pull = unsafe { Pull::from_bytes_unchecked_mut(self.pull.borrow_mut_data_unchecked()) };
        pull.set_version(PULL_VERSION);
        pull.set_bump(self.bump);
        pull.set_status(STATUS_PENDING);
        pull.set_count(self.count);
        pull.set_inventory_version(pool.inventory_version());
        pull.set_index(self.index);
        pull.set_deadline_slot(self.deadline_slot);
        pull.set_pool(*self.pool.key());
        pull.set_buyer(*self.buyer.key());
        pull.set_client_seed(*self.client_seed);

        pool.set_pending_draws(self.pending_draws);
        pool.set_next_index(self.next_index);

        Transfer {
            from: self.buyer_ata,
            to: self.vault,
            authority: self.buyer,
            amount: self.amount,
        }
        .invoke()
    }
}
