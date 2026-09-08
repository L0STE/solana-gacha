use crate::{errors::GachaError, helpers::token_account, state::Pool};
use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};

/// Return one prize for the pool authority's signed price, then restock it.
/// Quotes are reusable until expiry. Seller authorizes the return; payer funds rent.
pub(crate) struct Buyback<'a> {
    payer: &'a AccountInfo,
    seller: &'a AccountInfo,
    pool: &'a AccountInfo,
    item: &'a AccountInfo,
    asset: &'a AccountInfo,
    collection: &'a AccountInfo,
    core_program: &'a AccountInfo,
    system_program: &'a AccountInfo,
    vault: &'a AccountInfo,
    destination: &'a AccountInfo,
    tier: u8,
    price: u64,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Buyback<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        let [payer, seller, pool, item, asset, collection, vault, destination, core_program, _token, system_program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        if data.len() != 81 {
            return Err(ProgramError::InvalidInstructionData);
        }
        if !payer.is_signer() || !seller.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !payer.is_writable()
            || !pool.is_writable()
            || !item.is_writable()
            || !asset.is_writable()
        {
            return Err(GachaError::NotMutable.into());
        }
        crate::state::check_pool(pool)?;
        let tier = data[0];
        let price = u64::from_le_bytes(data[1..9].try_into().unwrap());
        let expires_at = i64::from_le_bytes(data[9..17].try_into().unwrap());
        let signature = data[17..].try_into().unwrap();
        let header = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
        if header.status() != crate::constants::POOL_ACTIVE {
            return Err(GachaError::InvalidPoolStatus.into());
        }
        if price == 0 || tier >= header.tier_count() {
            return Err(GachaError::InvalidQuote.into());
        }
        if Clock::get()?.unix_timestamp >= expires_at {
            return Err(GachaError::QuoteExpired.into());
        }
        let message = gacha_core::buyback_message(pool.key(), asset.key(), price, expires_at, tier);
        brine_ed25519::verify_strict(
            &brine_ed25519::Address::new_from_array(*header.authority()),
            signature,
            &[&message],
        )
        .map_err(|_| GachaError::InvalidQuote)?;

        let (owner, mint, _) = token_account(destination)?;
        if owner != seller.key() || mint != header.payment_mint() || vault.key() != header.vault() {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        Ok(Self {
            payer,
            seller,
            pool,
            item,
            asset,
            collection,
            core_program,
            system_program,
            vault,
            destination,
            tier,
            price,
        })
    }
}

impl Buyback<'_> {
    pub(crate) const DISCRIMINATOR: u8 = 12;

    pub(crate) fn process(self) -> ProgramResult {
        crate::assets::Transfer {
            payer: self.payer,
            authority: self.seller,
            new_owner: self.pool,
            asset: self.asset,
            collection: self.collection,
            core_program: self.core_program,
            system_program: self.system_program,
        }
        .invoke_signed(&[])?;
        crate::inventory::restock(
            self.payer,
            self.pool,
            self.item,
            self.asset.key(),
            self.tier,
        )?;
        crate::helpers::pay_surplus(self.pool, self.vault, self.destination, self.price)
    }
}
