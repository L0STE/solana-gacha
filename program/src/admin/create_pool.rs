use crate::constants::{MAX_COUNT, MAX_TIERS, POOL_LEN, POOL_SEED};
use crate::errors::GachaError;
use crate::events::CreatePoolEvent;
use crate::helpers::{ata, check_uninitialized, create_pda};
use crate::state::{Load, Pool};
use core::mem::size_of;
use pinocchio::log::sol_log;
use pinocchio::pubkey::create_program_address;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Create Pool
///
/// Create a paused banner: fixed tier odds, price, settle deadline and the
/// operator whose Ed25519 key is the VRF key.
///
/// > Create the Pool account at its PDA
/// > Write the fixed parameters and tier weights
///
/// Accounts:
///
/// 1. authority:           [signer, mut]   pays rent
/// 2. pool:                [mut]           PDA [POOL_SEED, authority, id]
/// 3. operator:                            the VRF public key
/// 4. payment_mint:
/// 5. vault:                               the pool's ATA for payment_mint
/// 6. system_program:      [executable]
/// 7. event_authority:
/// 8. program:             [executable]    this program, for the event CPI
///
/// Parameters:
/// 1. id: u64,
/// 2. price: u64,
/// 3. deadline_slots: u64,
/// 4. bond_per_draw: u64,          // nonzero timeout penalty per draw
/// 5. tier_count: u8,
/// 6. weights: [u32; 8],           // relative weights, low tier first
/// 7. bump: u8,                    // pool PDA bump, one fixed-cost derivation
///
/// Note: Collateral is an ordinary SPL transfer into the vault; the SDKs compose one
/// in the same transaction. The vault itself is not read: anyone can create the ATA,
/// and its owner and mint are fixed by the derivation.
///
/// Account Checks:
/// - Authority: signer
/// - Pool: writable, empty system account at the PDA for (authority, id, bump)
/// - Operator: not validated; an invalid key only makes this authority's own pool
///   unsettleable, which refunds every buyer with the penalty
/// - Vault: the pool's ATA address for payment_mint
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Instruction Checks:
/// - 1 ≤ tier_count ≤ 8 and every used weight nonzero
/// - Price, penalty and deadline nonzero; payment plus penalty fits ten draws
///
/// Event Data:
/// - discriminator: u8, (255u8, 0u8)
/// - pool: Pubkey,
pub struct CreatePoolAccounts<'a> {
    pub authority: &'a AccountInfo,
    pub pool: &'a AccountInfo,
    pub operator: &'a AccountInfo,
    pub payment_mint: &'a AccountInfo,
    pub vault: &'a AccountInfo,
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for CreatePoolAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, pool, operator, payment_mint, vault, _system_program, event_authority, program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !pool.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        check_uninitialized(pool)?;

        // Return the accounts
        Ok(Self {
            authority,
            pool,
            operator,
            payment_mint,
            vault,
            event_authority,
            program,
        })
    }
}

pub struct CreatePoolInstructionData {
    pub id: u64,
    pub price: u64,
    pub deadline_slots: u64,
    pub bond_per_draw: u64,
    pub tier_count: u8,
    pub weights: [u32; MAX_TIERS],
    pub bump: u8,
}

impl<'a> TryFrom<&'a [u8]> for CreatePoolInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len().ne(&(size_of::<u64>() * 4
            + size_of::<u8>()
            + size_of::<[u32; MAX_TIERS]>()
            + size_of::<u8>()))
        {
            return Err(ProgramError::InvalidInstructionData);
        }

        let id = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let price = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let deadline_slots = u64::from_le_bytes(data[16..24].try_into().unwrap());
        let bond_per_draw = u64::from_le_bytes(data[24..32].try_into().unwrap());
        let tier_count = data[32];
        let weights: [u32; MAX_TIERS] = core::array::from_fn(|i| {
            u32::from_le_bytes(data[33 + i * 4..37 + i * 4].try_into().unwrap())
        });
        let bump = data[65];

        // Instruction Checks
        if tier_count == 0 || tier_count as usize > MAX_TIERS {
            return Err(GachaError::InvalidPoolParams.into());
        }
        if weights[..tier_count as usize].contains(&0) {
            return Err(GachaError::InvalidPoolParams.into());
        }
        if price == 0 || bond_per_draw == 0 || deadline_slots == 0 {
            return Err(GachaError::InvalidPoolParams.into());
        }
        if price
            .checked_add(bond_per_draw)
            .and_then(|amount| amount.checked_mul(MAX_COUNT as u64))
            .is_none()
        {
            return Err(GachaError::InvalidPoolParams.into());
        }

        Ok(Self {
            id,
            price,
            deadline_slots,
            bond_per_draw,
            tier_count,
            weights,
            bump,
        })
    }
}

pub struct CreatePool<'a> {
    pub accounts: CreatePoolAccounts<'a>,
    pub instruction_data: CreatePoolInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for CreatePool<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        sol_log("Create Pool");

        let accounts = CreatePoolAccounts::try_from(accounts)?;
        let instruction_data = CreatePoolInstructionData::try_from(data)?;

        // Return the initialized struct
        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> CreatePool<'a> {
    pub const DISCRIMINATOR: &'a u8 = &0;

    pub fn process(&mut self) -> ProgramResult {
        // The pool must be the PDA for (authority, id, bump) and the vault its ATA
        let pool_key = create_program_address(
            &[
                POOL_SEED,
                self.accounts.authority.key(),
                &self.instruction_data.id.to_le_bytes(),
                &[self.instruction_data.bump],
            ],
            &crate::ID,
        )
        .map_err(|_| GachaError::InvalidSeeds)?;
        if pool_key.ne(self.accounts.pool.key()) {
            return Err(GachaError::InvalidSeeds.into());
        }
        if self
            .accounts
            .vault
            .key()
            .ne(&ata(&pool_key, self.accounts.payment_mint.key()))
        {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        // Create the Pool account
        let seeds = Pool::seeds(
            self.accounts.authority.key(),
            self.instruction_data.id,
            self.instruction_data.bump,
        );
        create_pda(
            self.accounts.authority,
            self.accounts.pool,
            POOL_LEN,
            &seeds.as_seeds(),
        )?;

        // Populate it, paused until the authority has stocked it
        let pool = Pool::load_new(self.accounts.pool)?;
        pool.set_inner(
            self.instruction_data.bump,
            self.accounts.authority.key(),
            self.accounts.operator.key(),
            self.accounts.payment_mint.key(),
            self.accounts.vault.key(),
            self.instruction_data.id,
            self.instruction_data.price,
            self.instruction_data.deadline_slots,
            self.instruction_data.bond_per_draw,
            &self.instruction_data.weights[..self.instruction_data.tier_count as usize],
        );

        // Log the Create Pool Event
        CreatePoolEvent {
            pool: self.accounts.pool.key(),
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
