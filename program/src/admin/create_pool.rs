use crate::constants::*;
use crate::errors::GachaError;
use crate::helpers::{ata, create_pda, token_account};
use crate::state::Pool;
use pinocchio::{
    account_info::AccountInfo, instruction::Seed, program_error::ProgramError,
    pubkey::find_program_address, ProgramResult,
};
use pinocchio_token::instructions::Transfer;

/// # CreatePool
///
/// Open a banner: fixed tier odds, price, settle deadline and the operator
/// whose Ed25519 key is the VRF key. The authority posts the bond in the same
/// instruction.
///
/// Accounts:
///
/// 1. authority:      [signer, mut]   pays rent and the bond
/// 2. pool:           [mut]           PDA [POOL_SEED, authority, id]
/// 3. operator:                       the VRF public key
/// 4. payment_mint:
/// 5. vault:          [mut]           the pool's ATA for payment_mint, pre-created
/// 6. authority_ata:  [mut]
/// 7. token_program:  [executable]
/// 8. system_program: [executable]
///
/// Parameters:
/// 1. id: u64,
/// 2. price: u64,
/// 3. deadline_slots: u64,
/// 4. bond_per_draw: u64,      // nonzero timeout penalty per draw
/// 5. bond: u64,               // initial collateral; ordinary transfers can add more
/// 6. tier_count: u8,
/// 7. weights: [u32; 8],      // relative weights, low tier first
///
/// Account Checks:
/// - Authority: signer; the PDA derivation binds the pool to it
/// - Pool: must be the PDA for (authority, id); created here
/// - Vault: must be the ATA of the pool for payment_mint, owned by the pool
/// - Operator: valid ECVRF public key
///
/// Instruction Checks:
/// - 1 ≤ tier_count ≤ 8, every used weight nonzero; nonzero price, penalty
///   and deadline; payment plus penalty must fit for up to ten draws
struct CreatePoolAccounts<'a> {
    authority: &'a AccountInfo,
    pool: &'a AccountInfo,
    operator: &'a AccountInfo,
    payment_mint: &'a AccountInfo,
    vault: &'a AccountInfo,
    authority_ata: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for CreatePoolAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, pool, operator, payment_mint, vault, authority_ata, _token_program, _system_program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !pool.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        let (owner, mint, _) = token_account(vault)?;
        if owner.ne(pool.key()) || mint.ne(payment_mint.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        Ok(Self {
            authority,
            pool,
            operator,
            payment_mint,
            vault,
            authority_ata,
        })
    }
}

struct CreatePoolArgs {
    id: u64,
    price: u64,
    deadline_slots: u64,
    bond_per_draw: u64,
    bond: u64,
    tier_count: u8,
    weights: [u32; MAX_TIERS],
}

impl<'a> TryFrom<&'a [u8]> for CreatePoolArgs {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() != 73 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let u64_at = |i: usize| u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
        let mut weights = [0u32; MAX_TIERS];
        for (i, weight) in weights.iter_mut().enumerate() {
            *weight = u32::from_le_bytes(data[41 + i * 4..45 + i * 4].try_into().unwrap());
        }
        Ok(Self {
            id: u64_at(0),
            price: u64_at(8),
            deadline_slots: u64_at(16),
            bond_per_draw: u64_at(24),
            bond: u64_at(32),
            tier_count: data[40],
            weights,
        })
    }
}

pub(crate) struct CreatePool<'a> {
    accounts: CreatePoolAccounts<'a>,
    args: CreatePoolArgs,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for CreatePool<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("CreatePool");

        let accounts = CreatePoolAccounts::try_from(accounts)?;
        let args = CreatePoolArgs::try_from(data)?;

        Ok(Self { accounts, args })
    }
}

impl<'a> CreatePool<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 0;

    pub(crate) fn process(self) -> ProgramResult {
        let accounts = &self.accounts;
        let config = &self.args;

        let tier_count = config.tier_count as usize;
        if tier_count == 0
            || tier_count > MAX_TIERS
            || config.weights[..tier_count].contains(&0)
            || config.price == 0
            || config.bond_per_draw == 0
            || config.deadline_slots == 0
            || config
                .price
                .checked_add(config.bond_per_draw)
                .and_then(|amount| amount.checked_mul(MAX_COUNT as u64))
                .is_none()
        {
            return Err(GachaError::InvalidPoolParams.into());
        }
        solana_ecvrf::PublicKey(*accounts.operator.key())
            .validate()
            .map_err(|_| GachaError::InvalidOperator)?;

        let id = config.id.to_le_bytes();
        let (pool_key, bump) =
            find_program_address(&[POOL_SEED, accounts.authority.key(), &id], &crate::ID);
        if pool_key.ne(accounts.pool.key()) {
            return Err(GachaError::InvalidAccountOwner.into());
        }
        if accounts
            .vault
            .key()
            .ne(&ata(&pool_key, accounts.payment_mint.key()))
        {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        let bump_seed = [bump];
        create_pda(
            accounts.authority,
            accounts.pool,
            POOL_LEN,
            &[
                Seed::from(POOL_SEED),
                Seed::from(accounts.authority.key()),
                Seed::from(&id),
                Seed::from(&bump_seed),
            ],
        )?;

        let pool =
            unsafe { Pool::from_bytes_unchecked_mut(accounts.pool.borrow_mut_data_unchecked()) };
        pool.set_version(POOL_VERSION);
        pool.set_bump(bump);
        pool.set_tier_count(config.tier_count);
        pool.set_authority(*accounts.authority.key());
        pool.set_operator(*accounts.operator.key());
        pool.set_payment_mint(*accounts.payment_mint.key());
        pool.set_vault(*accounts.vault.key());
        pool.set_id(config.id);
        pool.set_price(config.price);
        pool.set_deadline_slots(config.deadline_slots);
        pool.set_bond_per_draw(config.bond_per_draw);
        for (tier, &weight) in pool.tiers_mut().iter_mut().zip(&config.weights) {
            tier.set_weight(weight);
        }

        if config.bond > 0 {
            Transfer {
                from: accounts.authority_ata,
                to: accounts.vault,
                authority: accounts.authority,
                amount: config.bond,
            }
            .invoke()?;
        }
        Ok(())
    }
}
