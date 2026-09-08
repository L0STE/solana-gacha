use crate::constants::*;
use crate::errors::GachaError;
use crate::helpers::{ata, create_pda, token_account};
use crate::inventory::{self, Inventory};
use crate::state::{Item, Pool};
use pinocchio::{
    account_info::AccountInfo,
    instruction::Seed,
    program_error::ProgramError,
    pubkey::find_program_address,
    sysvars::{rent::Rent, Sysvar},
    ProgramResult,
};
use pinocchio_token::instructions::Transfer;

/// # DepositItem
///
/// Put one raw SPL token unit into a tier. It moves into the pool's token account
/// for its mint and an `Item` account records `(tier, position, mint)`,
/// appended at the pool's never-reused global inventory position.
///
/// Accounts:
///
/// 1. authority:      [signer, mut]   pool authority, pays item rent
/// 2. pool:           [mut]
/// 3. item:           [mut]           PDA [ITEM_SEED, pool, tier, position]
/// 4. mint:
/// 5. source:         [mut]           authority's token account for mint
/// 6. pool_ata:       [mut]           pool's ATA for mint, pre-created
/// 7. token_program:  [executable]
/// 8. system_program: [executable]
///
/// Parameters:
/// 1. tier: u8,
///
/// Account Checks:
/// - Authority: signer, equals pool.authority
/// - Pool: owner, length and version via Pool::check; writable
/// - Item: must be the PDA for (pool, tier, inventory_version); created here
/// - Pool ATA: must be the pool's ATA for mint, distinct from the payment vault
/// - Source, mint: the transfer fails on a mismatch
///
/// Instruction Checks:
/// - tier < tier_count
struct DepositItemAccounts<'a> {
    authority: &'a AccountInfo,
    pool: &'a AccountInfo,
    item: &'a AccountInfo,
    mint: &'a AccountInfo,
    source: &'a AccountInfo,
    pool_ata: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for DepositItemAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [authority, pool, item, mint, source, pool_ata, _token_program, _system_program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !pool.is_writable() || !item.is_writable() {
            return Err(GachaError::NotMutable.into());
        }
        Pool::check(pool)?;
        let header = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
        if header.authority().ne(authority.key()) {
            return Err(GachaError::InvalidAuthority.into());
        }
        let (owner, ata_mint, _) = token_account(pool_ata)?;
        if owner.ne(pool.key())
            || ata_mint.ne(mint.key())
            || pool_ata.key().eq(header.vault())
            || pool_ata.key().ne(&ata(pool.key(), mint.key()))
        {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        Ok(Self {
            authority,
            pool,
            item,
            mint,
            source,
            pool_ata,
        })
    }
}

pub(crate) struct DepositItem<'a> {
    accounts: DepositItemAccounts<'a>,
    tier: u8,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for DepositItem<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("DepositItem");

        let accounts = DepositItemAccounts::try_from(accounts)?;
        let [tier] = data else {
            return Err(ProgramError::InvalidInstructionData);
        };

        Ok(Self {
            accounts,
            tier: *tier,
        })
    }
}

impl<'a> DepositItem<'a> {
    pub(crate) const DISCRIMINATOR: u8 = 1;

    pub(crate) fn process(self) -> ProgramResult {
        let accounts = &self.accounts;
        let pool = unsafe { Pool::from_bytes_unchecked(accounts.pool.borrow_data_unchecked()) };
        let tier_index = self.tier;
        if tier_index >= pool.tier_count() {
            return Err(GachaError::InvalidTier.into());
        }
        let position = pool.inventory_version();
        let next_position = position
            .checked_add(1)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        let remaining = pool.tiers()[tier_index as usize]
            .remaining()
            .checked_add(1)
            .ok_or(ProgramError::ArithmeticOverflow)?;

        let tier_seed = [tier_index];
        let position_seed = position.to_le_bytes();
        let (item_key, bump) = find_program_address(
            &[ITEM_SEED, accounts.pool.key(), &tier_seed, &position_seed],
            &crate::ID,
        );
        if item_key.ne(accounts.item.key()) {
            return Err(GachaError::InvalidItem.into());
        }
        let bump_seed = [bump];
        create_pda(
            accounts.authority,
            accounts.item,
            ITEM_LEN,
            &[
                Seed::from(ITEM_SEED),
                Seed::from(accounts.pool.key()),
                Seed::from(&tier_seed),
                Seed::from(&position_seed),
                Seed::from(&bump_seed),
            ],
        )?;

        let space = POOL_LEN + inventory::space(next_position);
        if space != accounts.pool.data_len() {
            let missing = Rent::get()?
                .minimum_balance(space)
                .saturating_sub(accounts.pool.lamports());
            if missing > 0 {
                pinocchio_system::instructions::Transfer {
                    from: accounts.authority,
                    to: accounts.pool,
                    lamports: missing,
                }
                .invoke()?;
            }
            accounts.pool.realloc(space, false)?;
        }

        let item =
            unsafe { Item::from_bytes_unchecked_mut(accounts.item.borrow_mut_data_unchecked()) };
        item.set_version(ITEM_VERSION);
        item.set_bump(bump);
        item.set_tier(tier_index);
        item.set_position(position);
        item.set_pool(*accounts.pool.key());
        item.set_mint(*accounts.mint.key());

        let data = unsafe { accounts.pool.borrow_mut_data_unchecked() };
        let (header, index) = data.split_at_mut(POOL_LEN);
        let pool = unsafe { Pool::from_bytes_unchecked_mut(header) };
        Inventory::new(index).append(position, tier_index);
        pool.set_inventory_version(next_position);
        let tier = &mut pool.tiers_mut()[tier_index as usize];
        tier.set_remaining(remaining);

        Transfer {
            from: accounts.source,
            to: accounts.pool_ata,
            authority: accounts.authority,
            amount: 1,
        }
        .invoke()
    }
}
