use crate::assets::Transfer;
use crate::constants::POOL_ACTIVE;
use crate::errors::GachaError;
use crate::events::BuybackEvent;
use crate::helpers::{pay, spendable, TokenAccount};
use crate::inventory::{check_restock, restock};
use crate::state::{Load, Pool};
use core::mem::size_of;
use pinocchio::log::sol_log;
use pinocchio::sysvars::{clock::Clock, Sysvar};
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Buyback
///
/// Return one prize for the pool authority's signed price, then restock it.
/// Quotes are reusable by any holder until expiry.
///
/// > Transfer the asset from the seller to the pool PDA
/// > Append a fresh inventory position for it
/// > Pay the seller from vault surplus, never from pending refund reserves
///
/// Accounts:
///
/// 1. payer:               [signer, mut]   funds Item and index rent
/// 2. seller:              [signer]        current holder, authorizes the return
/// 3. pool:                [mut]
/// 4. item:                [mut]           PDA for the pool's next position
/// 5. asset:               [mut]
/// 6. collection:                          the asset's collection, or the Core program
/// 7. vault:               [mut]
/// 8. destination:         [mut]           seller's payment token account
/// 9. core_program:        [executable]
/// 10. token_program:      [executable]
/// 11. system_program:     [executable]
/// 12. event_authority:
/// 13. program:            [executable]    this program, for the event CPI
///
/// Parameters:
/// 1. tier: u8,
/// 2. price: u64,
/// 3. expires_at: i64,             // Unix timestamp in seconds
/// 4. signature: [u8; 64],         // authority's Ed25519 signature over `buyback_message`
/// 5. bump: u8,                    // item PDA bump for the pool's next position
///
/// Account Checks:
/// - Payer, Seller: signers
/// - Pool: writable, deserialized, active
/// - Item: writable; the PDA and emptiness checks need the tier, so they run in process
/// - Asset, Collection, CoreProgram, SystemProgram: no need to check since Core
///   validates the asset, the seller's signature and the collection on transfer
/// - Vault: equal to pool.vault
/// - Destination: owned by the seller; no need to check the mint since the payment fails
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Instruction Checks:
/// - ExpiresAt: not yet reached
/// - Tier, Signature and Price: need the pool, so they run in process
///
/// Event Data:
/// - discriminator: u8, (255u8, 12u8)
/// - pool: Pubkey,
/// - asset: Pubkey,
/// - seller: Pubkey,
/// - price: u64,
/// - position: u32,
pub struct BuybackAccounts<'a> {
    pub payer: &'a AccountInfo,
    pub seller: &'a AccountInfo,
    pub pool: &'a AccountInfo,
    pub item: &'a AccountInfo,
    pub asset: &'a AccountInfo,
    pub collection: &'a AccountInfo,
    pub vault: &'a AccountInfo,
    pub destination: &'a AccountInfo,
    pub core_program: &'a AccountInfo,
    pub system_program: &'a AccountInfo,
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for BuybackAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [payer, seller, pool, item, asset, collection, vault, destination, core_program, _token_program, system_program, event_authority, program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !payer.is_signer() || !seller.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !pool.is_writable() || !item.is_writable() {
            return Err(GachaError::NotMutable.into());
        }

        let pool_data = Pool::load(pool)?;
        if pool_data.status().ne(&POOL_ACTIVE) {
            return Err(GachaError::InvalidPoolStatus.into());
        }
        if pool_data.vault().ne(vault.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }
        let destination_data = TokenAccount::load(destination)?;
        if destination_data.owner.ne(seller.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }

        // Return the accounts
        Ok(Self {
            payer,
            seller,
            pool,
            item,
            asset,
            collection,
            vault,
            destination,
            core_program,
            system_program,
            event_authority,
            program,
        })
    }
}

pub struct BuybackInstructionData<'a> {
    pub tier: u8,
    pub price: u64,
    pub expires_at: i64,
    pub signature: &'a [u8; 64],
    pub bump: u8,
}

impl<'a> TryFrom<&'a [u8]> for BuybackInstructionData<'a> {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len().ne(&(size_of::<u8>()
            + size_of::<u64>()
            + size_of::<i64>()
            + size_of::<[u8; 64]>()
            + size_of::<u8>()))
        {
            return Err(ProgramError::InvalidInstructionData);
        }

        let tier = data[0];
        let price = u64::from_le_bytes(data[1..9].try_into().unwrap());
        let expires_at = i64::from_le_bytes(data[9..17].try_into().unwrap());
        let signature = data[17..81].try_into().unwrap();
        let bump = data[81];

        // Instruction Checks
        if Clock::get()?.unix_timestamp >= expires_at {
            return Err(GachaError::QuoteExpired.into());
        }

        Ok(Self {
            tier,
            price,
            expires_at,
            signature,
            bump,
        })
    }
}

pub struct Buyback<'a> {
    pub accounts: BuybackAccounts<'a>,
    pub instruction_data: BuybackInstructionData<'a>,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Buyback<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        sol_log("Buyback");

        let accounts = BuybackAccounts::try_from(accounts)?;
        let instruction_data = BuybackInstructionData::try_from(data)?;

        // Return the initialized struct
        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Buyback<'a> {
    pub const DISCRIMINATOR: &'a u8 = &12;

    pub fn process(&mut self) -> ProgramResult {
        // The quote must be the authority's signature over these exact terms
        let pool = Pool::load(self.accounts.pool)?;
        if self.instruction_data.tier >= pool.tier_count() {
            return Err(GachaError::InvalidQuote.into());
        }
        let message = gacha_core::buyback_message(
            self.accounts.pool.key(),
            self.accounts.asset.key(),
            self.instruction_data.price,
            self.instruction_data.expires_at,
            self.instruction_data.tier,
        );
        brine_ed25519::verify_strict(
            &brine_ed25519::Address::new_from_array(*pool.authority()),
            self.instruction_data.signature,
            &[&message],
        )
        .map_err(|_| GachaError::InvalidQuote)?;

        // Only surplus above every pending refund pays for buybacks
        if self.instruction_data.price > spendable(pool, self.accounts.vault)? {
            return Err(GachaError::InsufficientBalance.into());
        }

        // The item must be the empty PDA for the next position
        let position = check_restock(
            pool,
            self.accounts.pool.key(),
            self.accounts.item,
            self.instruction_data.tier,
            self.instruction_data.bump,
        )?;

        // Take custody of the asset
        Transfer {
            payer: self.accounts.payer,
            authority: self.accounts.seller,
            new_owner: self.accounts.pool,
            asset: self.accounts.asset,
            collection: self.accounts.collection,
            core_program: self.accounts.core_program,
            system_program: self.accounts.system_program,
        }
        .invoke_signed(&[])?;

        // Create the Item and append its position to the availability index
        restock(
            self.accounts.payer,
            self.accounts.pool,
            self.accounts.item,
            self.accounts.asset.key(),
            self.instruction_data.tier,
            self.instruction_data.bump,
        )?;

        // Pay the seller
        pay(
            self.accounts.pool,
            self.accounts.vault,
            self.accounts.destination,
            self.instruction_data.price,
        )?;

        // Log the Buyback Event
        BuybackEvent {
            pool: self.accounts.pool.key(),
            asset: self.accounts.asset.key(),
            seller: self.accounts.seller.key(),
            price: self.instruction_data.price,
            position,
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
