use crate::constants::{MAX_COUNT, POOL_ACTIVE, PULL_LEN, PULL_SEED};
use crate::errors::GachaError;
use crate::events::BuyEvent;
use crate::helpers::{check_uninitialized, create_pda, TokenAccount};
use crate::state::{Load, Pool, Pull};
use core::mem::size_of;
use pinocchio::log::sol_log;
use pinocchio::pubkey::create_program_address;
use pinocchio::sysvars::{clock::Clock, Sysvar};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};
use pinocchio_token::instructions::Transfer;

/// # Buy
///
/// Commit to a pull: pay `price × count` into escrow and record the buyer's
/// seed. Randomness is `VRF(operator, SHA-256(pull ‖ seed))`; the operator can
/// compute it once the seed is known. The signed inventory version pins the
/// candidate list before that seed is shared; later deposits cannot enter it.
///
/// > Create the Pull account at its seed-derived PDA
/// > Record buyer, seed, pinned inventory version, FIFO index and deadline
/// > Reserve the draws and transfer the payment into the vault
///
/// Accounts:
///
/// 1. buyer:               [signer, mut]   pays rent and the price
/// 2. pool:                [mut]
/// 3. pull:                [mut]           PDA [PULL_SEED, pool, client_seed]
/// 4. buyer_ata:           [mut]
/// 5. vault:               [mut]
/// 6. token_program:       [executable]
/// 7. system_program:      [executable]
/// 8. event_authority:
/// 9. program:             [executable]    this program, for the event CPI
///
/// Parameters:
/// 1. count: u8,
/// 2. client_seed: [u8; 32],
/// 3. inventory_version: u32,      // expected deposit count, read before signing
/// 4. bump: u8,                    // pull PDA bump, one fixed-cost derivation
///
/// Note: The FIFO index is assigned here, at execution, so concurrent buyers never
/// contend for one Pull address.
///
/// Account Checks:
/// - Buyer: signer
/// - Pool: writable, deserialized, active
/// - Pull: writable, empty system account; the PDA check needs the seed, so it runs in process
/// - BuyerAta: no need to check since the transfer fails on a wrong mint or owner
/// - Vault: equal to pool.vault
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Instruction Checks:
/// - Count: 1 ≤ count ≤ 10
/// - InventoryVersion, stock and collateral: need the pool, so they run in process
///
/// Event Data:
/// - discriminator: u8, (255u8, 10u8)
/// - pool: Pubkey,
/// - pull: Pubkey,
/// - buyer: Pubkey,
/// - index: u64,
/// - count: u8,
pub struct BuyAccounts<'a> {
    pub buyer: &'a AccountInfo,
    pub pool: &'a AccountInfo,
    pub pull: &'a AccountInfo,
    pub buyer_ata: &'a AccountInfo,
    pub vault: &'a AccountInfo,
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for BuyAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [buyer, pool, pull, buyer_ata, vault, _token_program, _system_program, event_authority, program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !buyer.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !pool.is_writable() || !pull.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        check_uninitialized(pull)?;

        let pool_data = Pool::load(pool)?;
        if pool_data.status().ne(&POOL_ACTIVE) {
            return Err(GachaError::InvalidPoolStatus.into());
        }
        if pool_data.vault().ne(vault.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        // Return the accounts
        Ok(Self {
            buyer,
            pool,
            pull,
            buyer_ata,
            vault,
            event_authority,
            program,
        })
    }
}

pub struct BuyInstructionData<'a> {
    pub count: u8,
    pub client_seed: &'a [u8; 32],
    pub inventory_version: u32,
    pub bump: u8,
}

impl<'a> TryFrom<&'a [u8]> for BuyInstructionData<'a> {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data
            .len()
            .ne(&(size_of::<u8>() + size_of::<[u8; 32]>() + size_of::<u32>() + size_of::<u8>()))
        {
            return Err(ProgramError::InvalidInstructionData);
        }

        let count = data[0];
        let client_seed = data[1..33].try_into().unwrap();
        let inventory_version = u32::from_le_bytes(data[33..37].try_into().unwrap());
        let bump = data[37];

        // Instruction Checks
        if count == 0 || count as usize > MAX_COUNT {
            return Err(GachaError::InvalidCount.into());
        }

        Ok(Self {
            count,
            client_seed,
            inventory_version,
            bump,
        })
    }
}

pub struct Buy<'a> {
    pub accounts: BuyAccounts<'a>,
    pub instruction_data: BuyInstructionData<'a>,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Buy<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        sol_log("Buy");

        let accounts = BuyAccounts::try_from(accounts)?;
        let instruction_data = BuyInstructionData::try_from(data)?;

        // Return the initialized struct
        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Buy<'a> {
    pub const DISCRIMINATOR: &'a u8 = &10;

    pub fn process(&mut self) -> ProgramResult {
        // The pull must be the PDA for (pool, client_seed, bump)
        let pull_key = create_program_address(
            &[
                PULL_SEED,
                self.accounts.pool.key(),
                self.instruction_data.client_seed,
                &[self.instruction_data.bump],
            ],
            &crate::ID,
        )
        .map_err(|_| GachaError::InvalidSeeds)?;
        if pull_key.ne(self.accounts.pull.key()) {
            return Err(GachaError::InvalidSeeds.into());
        }

        // The candidate list the buyer signed for must be the current one
        let pool = Pool::load_mut(self.accounts.pool)?;
        if self
            .instruction_data
            .inventory_version
            .ne(&pool.inventory_version())
        {
            return Err(GachaError::InventoryChanged.into());
        }

        // FIFO makes candidate prefixes nested: every older pending draw can consume at
        // most one item in this prefix, so this reserves enough eligible stock for this
        // purchase even if no further deposits arrive
        let count = self.instruction_data.count as u64;
        let pending_draws = pool.pending_draws() + count;
        if pending_draws > pool.remaining() {
            return Err(GachaError::SoldOut.into());
        }

        // The vault must cover every pending refund plus penalty once this payment lands
        let amount = pool.payment(count)?;
        let vault = TokenAccount::load(self.accounts.vault)?;
        if vault.amount + amount < pool.refund_amount(pending_draws)? {
            return Err(GachaError::InsufficientBalance.into());
        }

        // Create the Pull account
        let seeds = Pull::seeds(
            self.accounts.pool.key(),
            self.instruction_data.client_seed,
            self.instruction_data.bump,
        );
        create_pda(
            self.accounts.buyer,
            self.accounts.pull,
            PULL_LEN,
            &seeds.as_seeds(),
        )?;

        // Populate it and take the next FIFO index
        let index = pool.next_index();
        let pull = Pull::load_new(self.accounts.pull)?;
        pull.set_inner(
            self.instruction_data.bump,
            self.instruction_data.count,
            pool.inventory_version(),
            index,
            Clock::get()?.slot + pool.deadline_slots(),
            self.accounts.pool.key(),
            self.accounts.buyer.key(),
            self.instruction_data.client_seed,
        );
        pool.set_pending_draws(pending_draws);
        pool.set_next_index(index + 1);

        // Escrow the payment
        Transfer {
            from: self.accounts.buyer_ata,
            to: self.accounts.vault,
            authority: self.accounts.buyer,
            amount,
        }
        .invoke()?;

        // Log the Buy Event
        BuyEvent {
            pool: self.accounts.pool.key(),
            pull: self.accounts.pull.key(),
            buyer: self.accounts.buyer.key(),
            index,
            count: self.instruction_data.count,
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
