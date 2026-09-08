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
/// Create a paused banner: fixed tier odds, price, settle deadline and the operator
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
pub(crate) struct CreatePool<'a> {
    authority: &'a AccountInfo,
    pool: &'a AccountInfo,
    operator: &'a AccountInfo,
    payment_mint: &'a AccountInfo,
    vault: &'a AccountInfo,
    authority_ata: &'a AccountInfo,
    id: u64,
    price: u64,
    deadline_slots: u64,
    bond_per_draw: u64,
    bond: u64,
    tier_count: u8,
    weights: [u32; MAX_TIERS],
    bump: u8,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for CreatePool<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("CreatePool");
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
        if data.len() != 73 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let u64_at = |i: usize| u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
        let id = u64_at(0);
        let price = u64_at(8);
        let deadline_slots = u64_at(16);
        let bond_per_draw = u64_at(24);
        let bond = u64_at(32);
        let tier_count = data[40];
        let mut weights = [0u32; MAX_TIERS];
        for (i, weight) in weights.iter_mut().enumerate() {
            *weight = u32::from_le_bytes(data[41 + i * 4..45 + i * 4].try_into().unwrap());
        }
        if tier_count == 0
            || tier_count as usize > MAX_TIERS
            || weights[..tier_count as usize].contains(&0)
            || price == 0
            || bond_per_draw == 0
            || deadline_slots == 0
            || price
                .checked_add(bond_per_draw)
                .and_then(|amount| amount.checked_mul(MAX_COUNT as u64))
                .is_none()
        {
            return Err(GachaError::InvalidPoolParams.into());
        }
        solana_ecvrf::PublicKey(*operator.key())
            .validate()
            .map_err(|_| GachaError::InvalidOperator)?;
        let (pool_key, bump) =
            find_program_address(&[POOL_SEED, authority.key(), &id.to_le_bytes()], &crate::ID);
        if pool_key.ne(pool.key()) {
            return Err(GachaError::InvalidAccountOwner.into());
        }
        if vault.key().ne(&ata(&pool_key, payment_mint.key())) {
            return Err(GachaError::InvalidTokenAddress.into());
        }
        Ok(Self {
            authority,
            pool,
            operator,
            payment_mint,
            vault,
            authority_ata,
            id,
            price,
            deadline_slots,
            bond_per_draw,
            bond,
            tier_count,
            weights,
            bump,
        })
    }
}

impl CreatePool<'_> {
    pub(crate) const DISCRIMINATOR: u8 = 0;

    pub(crate) fn process(self) -> ProgramResult {
        let id = self.id.to_le_bytes();
        let bump = [self.bump];
        create_pda(
            self.authority,
            self.pool,
            POOL_LEN,
            &[
                Seed::from(POOL_SEED),
                Seed::from(self.authority.key()),
                Seed::from(&id),
                Seed::from(&bump),
            ],
        )?;
        let pool = unsafe { Pool::from_bytes_unchecked_mut(self.pool.borrow_mut_data_unchecked()) };
        pool.set_version(POOL_VERSION);
        pool.set_bump(self.bump);
        pool.set_tier_count(self.tier_count);
        pool.set_status(POOL_PAUSED);
        pool.set_authority(*self.authority.key());
        pool.set_operator(*self.operator.key());
        pool.set_payment_mint(*self.payment_mint.key());
        pool.set_vault(*self.vault.key());
        pool.set_id(self.id);
        pool.set_price(self.price);
        pool.set_deadline_slots(self.deadline_slots);
        pool.set_bond_per_draw(self.bond_per_draw);
        for (tier, &weight) in pool.tiers_mut().iter_mut().zip(&self.weights) {
            tier.set_weight(weight);
        }
        if self.bond > 0 {
            Transfer {
                from: self.authority_ata,
                to: self.vault,
                authority: self.authority,
                amount: self.bond,
            }
            .invoke()?;
        }
        Ok(())
    }
}
