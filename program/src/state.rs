//! Account layouts and the pure draw helpers that mutate them.
//!
//! Every struct has alignment 1: fields are little-endian byte arrays decoded
//! on access, so a struct overlays raw account bytes at any offset, on chain
//! and off. `check` validates owner, length and version for an account of
//! that type; the handlers then take unchecked views.
//!
//! # Where the moving odds live
//!
//! Weights are fixed at creation. Each purchase pins an append-only inventory
//! prefix; earlier FIFO winners reduce its availability, later deposits do not
//! enter it. Each position's mint and tier are immutable. The pool's
//! variable-length suffix indexes availability; each draw needs only its
//! awarded Item account.

use crate::constants::*;
use crate::errors::GachaError;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, pubkey::Pubkey};

#[cold]
fn cold_marker() {}

#[inline(always)]
pub fn unlikely(b: bool) -> bool {
    if b {
        cold_marker();
    }
    b
}

/// Little-endian getters and setters for a byte-array field.
macro_rules! field {
    ($get:ident, $set:ident, $field:ident, u8) => {
        #[inline(always)]
        pub fn $get(&self) -> u8 {
            self.$field[0]
        }
        #[inline(always)]
        pub fn $set(&mut self, v: u8) {
            self.$field[0] = v;
        }
    };
    ($get:ident, $set:ident, $field:ident, $t:ty) => {
        #[inline(always)]
        pub fn $get(&self) -> $t {
            <$t>::from_le_bytes(self.$field)
        }
        #[inline(always)]
        pub fn $set(&mut self, v: $t) {
            self.$field = v.to_le_bytes();
        }
    };
}

macro_rules! key {
    ($get:ident, $set:ident, $field:ident) => {
        #[inline(always)]
        pub fn $get(&self) -> &Pubkey {
            &self.$field
        }
        #[inline(always)]
        pub fn $set(&mut self, v: Pubkey) {
            self.$field = v;
        }
    };
}

macro_rules! account {
    ($name:ident, $len:expr, $version:expr, $valid_len:expr) => {
        impl $name {
            pub fn check(account_info: &AccountInfo) -> Result<(), ProgramError> {
                if unlikely(!account_info.is_owned_by(&crate::ID)) {
                    return Err(GachaError::InvalidAccountOwner.into());
                }
                if unlikely(account_info.data_len() < $len) {
                    return Err(GachaError::InvalidAccountLength.into());
                }
                let this =
                    unsafe { Self::from_bytes_unchecked(account_info.borrow_data_unchecked()) };
                if unlikely(this.version() != $version) {
                    return Err(GachaError::InvalidVersion.into());
                }
                if unlikely(!($valid_len)(this, account_info.data_len())) {
                    return Err(GachaError::InvalidAccountLength.into());
                }
                Ok(())
            }

            /// # Safety
            /// `bytes` must cover the struct's fixed layout; all fields have
            /// alignment 1. Use `check` to validate an existing account's owner,
            /// length and version before reading it.
            #[inline(always)]
            pub unsafe fn from_bytes_unchecked(bytes: &[u8]) -> &Self {
                &*(bytes.as_ptr() as *const Self)
            }

            /// # Safety
            /// Same layout requirements as `from_bytes_unchecked`; the caller
            /// must also have exclusive access to the bytes.
            #[inline(always)]
            pub unsafe fn from_bytes_unchecked_mut(bytes: &mut [u8]) -> &mut Self {
                &mut *(bytes.as_mut_ptr() as *mut Self)
            }
        }
    };
}

/// One banner. 192-byte header, `MAX_TIERS` tiers, then an availability index.
#[repr(C)]
pub struct Pool {
    version: [u8; 1],
    bump: [u8; 1],
    tier_count: [u8; 1],
    _padding: [u8; 1],
    inventory_version: [u8; 4],
    authority: [u8; 32],
    operator: [u8; 32],
    payment_mint: [u8; 32],
    vault: [u8; 32],
    id: [u8; 8],
    price: [u8; 8],
    deadline_slots: [u8; 8],
    bond_per_draw: [u8; 8],
    /// Inventory and full timeout payments reserved for accepted purchases.
    pending_draws: [u8; 8],
    next_index: [u8; 8],
    next_settle: [u8; 8],
    tiers: [Tier; MAX_TIERS],
}

account!(Pool, POOL_LEN, POOL_VERSION, |pool: &Pool, len: usize| {
    len == POOL_LEN + crate::inventory::space(pool.inventory_version())
});

impl Pool {
    field!(version, set_version, version, u8);
    field!(bump, set_bump, bump, u8);
    field!(tier_count, set_tier_count, tier_count, u8);
    field!(
        inventory_version,
        set_inventory_version,
        inventory_version,
        u32
    );
    key!(authority, set_authority, authority);
    key!(operator, set_operator, operator);
    #[cfg(not(target_os = "solana"))]
    pub fn payment_mint(&self) -> &Pubkey {
        &self.payment_mint
    }
    pub fn set_payment_mint(&mut self, mint: Pubkey) {
        self.payment_mint = mint;
    }
    key!(vault, set_vault, vault);
    field!(id, set_id, id, u64);
    field!(price, set_price, price, u64);
    field!(deadline_slots, set_deadline_slots, deadline_slots, u64);
    field!(bond_per_draw, set_bond_per_draw, bond_per_draw, u64);
    field!(pending_draws, set_pending_draws, pending_draws, u64);
    field!(next_index, set_next_index, next_index, u64);
    field!(next_settle, set_next_settle, next_settle, u64);

    pub fn payment(&self, count: u64) -> Result<u64, ProgramError> {
        self.price()
            .checked_mul(count)
            .ok_or(ProgramError::ArithmeticOverflow)
    }

    /// Full payment plus the configured penalty; never prorated by liquidity.
    pub fn refund_amount(&self, count: u64) -> Result<u64, ProgramError> {
        self.price()
            .checked_add(self.bond_per_draw())
            .and_then(|amount| amount.checked_mul(count))
            .ok_or(ProgramError::ArithmeticOverflow)
    }

    pub fn remaining(&self) -> u64 {
        self.tiers()
            .iter()
            .map(|tier| tier.remaining() as u64)
            .sum()
    }

    #[inline(always)]
    pub fn tiers(&self) -> &[Tier] {
        &self.tiers[..self.tier_count() as usize]
    }

    #[inline(always)]
    pub fn tiers_mut(&mut self) -> &mut [Tier] {
        let count = self.tier_count() as usize;
        &mut self.tiers[..count]
    }

    /// Select among the still-available tiers in this purchase's candidate set.
    #[inline(always)]
    pub fn draw_tier(
        &self,
        hash: &[u8; 32],
        remaining: &[u32; MAX_TIERS],
    ) -> Result<u8, ProgramError> {
        let total: u64 = self
            .tiers()
            .iter()
            .zip(remaining)
            .filter(|(_, count)| **count > 0)
            .map(|(tier, _)| tier.weight() as u64)
            .sum();
        if unlikely(total == 0) {
            return Err(GachaError::SoldOut.into());
        }
        let mut roll = u64::from_le_bytes(hash[..8].try_into().unwrap()) % total;
        for (i, tier) in self.tiers().iter().enumerate() {
            if remaining[i] == 0 {
                continue;
            }
            let weight = tier.weight() as u64;
            if roll < weight {
                return Ok(i as u8);
            }
            roll -= weight;
        }
        Err(GachaError::SoldOut.into())
    }
}

/// `weight` is fixed; `remaining` changes on deposit and settlement.
#[repr(C)]
pub struct Tier {
    weight: [u8; 4],
    remaining: [u8; 4],
}

impl Tier {
    field!(weight, set_weight, weight, u32);
    field!(remaining, set_remaining, remaining, u32);
}

/// One deposited prize: a raw SPL token unit held in the pool's account for `mint`.
/// Keyed by `(pool, tier, position)`; `position` is a never-reused global
/// deposit index. No instruction can replace the mint at this position.
#[repr(C)]
pub struct Item {
    version: [u8; 1],
    bump: [u8; 1],
    tier: [u8; 1],
    _padding: [u8; 1],
    position: [u8; 4],
    pool: [u8; 32],
    mint: [u8; 32],
}

account!(Item, ITEM_LEN, ITEM_VERSION, |_: &Item, len: usize| {
    len == ITEM_LEN
});

impl Item {
    field!(version, set_version, version, u8);
    #[inline(always)]
    pub fn set_bump(&mut self, v: u8) {
        self.bump[0] = v;
    }
    field!(tier, set_tier, tier, u8);
    field!(position, set_position, position, u32);
    key!(pool, set_pool, pool);
    key!(mint, set_mint, mint);
}

/// One purchase: `count` draws revealed together. `outcomes` holds
/// `(tier, mint)` per draw once settled; tier becomes `DELIVERED` on delivery.
#[repr(C)]
pub struct Pull {
    version: [u8; 1],
    bump: [u8; 1],
    status: [u8; 1],
    count: [u8; 1],
    inventory_version: [u8; 4],
    index: [u8; 8],
    deadline_slot: [u8; 8],
    pool: [u8; 32],
    buyer: [u8; 32],
    client_seed: [u8; 32],
    outcomes: [[u8; OUTCOME_LEN]; MAX_COUNT],
}

account!(Pull, PULL_LEN, PULL_VERSION, |_: &Pull, len: usize| {
    len == PULL_LEN
});

impl Pull {
    field!(version, set_version, version, u8);
    #[inline(always)]
    pub fn set_bump(&mut self, v: u8) {
        self.bump[0] = v;
    }
    field!(status, set_status, status, u8);
    field!(count, set_count, count, u8);
    field!(
        inventory_version,
        set_inventory_version,
        inventory_version,
        u32
    );
    field!(index, set_index, index, u64);
    field!(deadline_slot, set_deadline_slot, deadline_slot, u64);
    key!(pool, set_pool, pool);
    key!(buyer, set_buyer, buyer);
    key!(client_seed, set_client_seed, client_seed);

    #[inline(always)]
    pub fn outcome(&self, i: usize) -> (u8, &Pubkey) {
        let outcome = &self.outcomes[i];
        // The fixed 33-byte entry holds a tier followed by an alignment-1 key.
        (outcome[0], unsafe {
            &*(outcome[1..].as_ptr() as *const Pubkey)
        })
    }

    #[inline(always)]
    pub fn set_outcome(&mut self, i: usize, tier: u8, mint: &Pubkey) {
        self.outcomes[i][0] = tier;
        self.outcomes[i][1..].copy_from_slice(mint);
    }

    #[inline(always)]
    pub fn mark_delivered(&mut self, i: usize) {
        self.outcomes[i][0] = DELIVERED;
    }

    #[inline(always)]
    pub fn all_delivered(&self) -> bool {
        self.outcomes[..self.count() as usize]
            .iter()
            .all(|outcome| outcome[0] == DELIVERED)
    }
}

/// Reads the owner and mint of an SPL token account without deserializing it.
/// `helpers::token_account` validates the 165-byte layout before calling this.
#[inline(always)]
pub fn token_account_owner_and_mint(data: &[u8]) -> (&Pubkey, &Pubkey) {
    unsafe {
        (
            &*(data.as_ptr().add(32) as *const Pubkey),
            &*(data.as_ptr() as *const Pubkey),
        )
    }
}

#[inline(always)]
pub fn token_account_amount(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[64..72].try_into().unwrap())
}

const _: () = assert!(core::mem::size_of::<Pool>() == POOL_LEN);
const _: () = assert!(core::mem::size_of::<Item>() == ITEM_LEN);
const _: () = assert!(core::mem::size_of::<Pull>() == PULL_LEN);
const _: () = assert!(core::mem::size_of::<Tier>() == TIER_LEN);
