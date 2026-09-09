//! Account layouts and the pure draw helpers that mutate them.
//!
//! Every struct has alignment 1: fields are little-endian byte arrays decoded
//! on access, so a struct overlays raw account bytes at any offset, on chain
//! and off. Callers validate owner, length and version before taking views.
//!
//! # Where the moving odds live
//!
//! Weights are fixed at creation. Each purchase pins an append-only inventory
//! prefix; earlier FIFO winners reduce its availability, later deposits do not
//! enter it. Each position's asset and tier are immutable. The pool's
//! variable-length suffix indexes availability; each draw needs only its
//! awarded Item account.

use crate::constants::*;
use crate::errors::GachaError;
use pinocchio::{instruction::Seed, program_error::ProgramError, pubkey::Pubkey};

/// Pool signer seeds `[POOL_SEED, authority, id, bump]`, owned so a handler
/// can build them once and sign with `as_seeds()`.
pub struct PoolSeeds {
    authority: Pubkey,
    id: [u8; 8],
    bump: [u8; 1],
}

impl PoolSeeds {
    #[inline(always)]
    pub fn as_seeds(&self) -> [Seed<'_>; 4] {
        [
            Seed::from(POOL_SEED),
            Seed::from(&self.authority),
            Seed::from(&self.id),
            Seed::from(&self.bump),
        ]
    }
}

/// Pull signer seeds `[PULL_SEED, pool, client_seed, bump]`.
pub struct PullSeeds {
    pool: Pubkey,
    client_seed: [u8; 32],
    bump: [u8; 1],
}

impl PullSeeds {
    #[inline(always)]
    pub fn as_seeds(&self) -> [Seed<'_>; 4] {
        [
            Seed::from(PULL_SEED),
            Seed::from(&self.pool),
            Seed::from(&self.client_seed),
            Seed::from(&self.bump),
        ]
    }
}

/// Item signer seeds `[ITEM_SEED, pool, tier, position, bump]`.
pub struct ItemSeeds {
    pool: Pubkey,
    tier: [u8; 1],
    position: [u8; 4],
    bump: [u8; 1],
}

impl ItemSeeds {
    #[inline(always)]
    pub fn as_seeds(&self) -> [Seed<'_>; 5] {
        [
            Seed::from(ITEM_SEED),
            Seed::from(&self.pool),
            Seed::from(&self.tier),
            Seed::from(&self.position),
            Seed::from(&self.bump),
        ]
    }
}

#[cold]
fn cold_marker() {}

#[inline(always)]
fn unlikely(b: bool) -> bool {
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
    ($name:ident) => {
        impl $name {
            /// # Safety
            /// `bytes` must cover the struct's fixed layout; all fields have
            /// alignment 1. The caller validates owner, length and version.
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
    status: [u8; 1],
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

account!(Pool);

impl Pool {
    field!(version, set_version, version, u8);
    field!(bump, set_bump, bump, u8);
    field!(tier_count, set_tier_count, tier_count, u8);
    field!(status, set_status, status, u8);
    field!(
        inventory_version,
        set_inventory_version,
        inventory_version,
        u32
    );
    key!(authority, set_authority, authority);
    key!(operator, set_operator, operator);
    key!(payment_mint, set_payment_mint, payment_mint);
    key!(vault, set_vault, vault);
    field!(id, set_id, id, u64);
    field!(price, set_price, price, u64);
    field!(deadline_slots, set_deadline_slots, deadline_slots, u64);
    field!(bond_per_draw, set_bond_per_draw, bond_per_draw, u64);
    field!(pending_draws, set_pending_draws, pending_draws, u64);
    field!(next_index, set_next_index, next_index, u64);
    field!(next_settle, set_next_settle, next_settle, u64);

    /// Write every field of a freshly created, paused pool.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub fn set_inner(
        &mut self,
        bump: u8,
        authority: &Pubkey,
        operator: &Pubkey,
        payment_mint: &Pubkey,
        vault: &Pubkey,
        id: u64,
        price: u64,
        deadline_slots: u64,
        bond_per_draw: u64,
        weights: &[u32],
    ) {
        self.set_version(POOL_VERSION);
        self.set_bump(bump);
        self.set_tier_count(weights.len() as u8);
        self.set_status(POOL_PAUSED);
        self.set_authority(*authority);
        self.set_operator(*operator);
        self.set_payment_mint(*payment_mint);
        self.set_vault(*vault);
        self.set_id(id);
        self.set_price(price);
        self.set_deadline_slots(deadline_slots);
        self.set_bond_per_draw(bond_per_draw);
        for (tier, &weight) in self.tiers_mut().iter_mut().zip(weights) {
            tier.set_weight(weight);
        }
    }

    #[inline(always)]
    pub fn seeds(authority: &Pubkey, id: u64, bump: u8) -> PoolSeeds {
        PoolSeeds {
            authority: *authority,
            id: id.to_le_bytes(),
            bump: [bump],
        }
    }

    /// This pool's own signer seeds.
    #[inline(always)]
    pub fn signer_seeds(&self) -> PoolSeeds {
        Self::seeds(self.authority(), self.id(), self.bump())
    }

    #[inline]
    pub fn payment(&self, count: u64) -> Result<u64, ProgramError> {
        self.price()
            .checked_mul(count)
            .ok_or(ProgramError::ArithmeticOverflow)
    }

    /// Full payment plus the configured penalty; never prorated by liquidity.
    #[inline]
    pub fn refund_amount(&self, count: u64) -> Result<u64, ProgramError> {
        self.price()
            .checked_add(self.bond_per_draw())
            .and_then(|amount| amount.checked_mul(count))
            .ok_or(ProgramError::ArithmeticOverflow)
    }

    #[inline]
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

    /// One draw: a still-available tier by weight, then a rank within that
    /// tier's remaining candidates. Rank order is by deposit position.
    #[inline(always)]
    pub fn draw(
        &self,
        hash: &[u8; 32],
        remaining: &[u32; MAX_TIERS],
    ) -> Result<(u8, u32), ProgramError> {
        let tier = self.draw_tier(hash, remaining)?;
        let rank =
            u64::from_le_bytes(hash[8..16].try_into().unwrap()) % remaining[tier as usize] as u64;
        Ok((tier, rank as u32))
    }

    #[inline(always)]
    fn draw_tier(&self, hash: &[u8; 32], remaining: &[u32; MAX_TIERS]) -> Result<u8, ProgramError> {
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

/// One deposited Core NFT whose owner is the pool PDA.
/// Keyed by `(pool, tier, position)`; `position` is a never-reused global
/// deposit index. No instruction can replace the asset at this position.
#[repr(C)]
pub struct Item {
    version: [u8; 1],
    bump: [u8; 1],
    tier: [u8; 1],
    _padding: [u8; 1],
    position: [u8; 4],
    pool: [u8; 32],
    asset: [u8; 32],
}

account!(Item);

impl Item {
    field!(version, set_version, version, u8);
    field!(bump, set_bump, bump, u8);
    /// Write every field of a freshly created item.
    #[inline(always)]
    pub fn set_inner(&mut self, bump: u8, tier: u8, position: u32, pool: &Pubkey, asset: &Pubkey) {
        self.set_version(ITEM_VERSION);
        self.set_bump(bump);
        self.set_tier(tier);
        self.set_position(position);
        self.set_pool(*pool);
        self.set_asset(*asset);
    }

    #[inline(always)]
    pub fn seeds(pool: &Pubkey, tier: u8, position: u32, bump: u8) -> ItemSeeds {
        ItemSeeds {
            pool: *pool,
            tier: [tier],
            position: position.to_le_bytes(),
            bump: [bump],
        }
    }
    field!(tier, set_tier, tier, u8);
    field!(position, set_position, position, u32);
    key!(pool, set_pool, pool);
    key!(asset, set_asset, asset);
}

/// One purchase: `count` draws revealed together. `outcomes` holds
/// `(tier, asset)` per draw once settled; tier becomes `DELIVERED` on delivery.
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

account!(Pull);

impl Pull {
    field!(version, set_version, version, u8);
    field!(bump, set_bump, bump, u8);
    /// Write every field of a freshly created, pending pull.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub fn set_inner(
        &mut self,
        bump: u8,
        count: u8,
        inventory_version: u32,
        index: u64,
        deadline_slot: u64,
        pool: &Pubkey,
        buyer: &Pubkey,
        client_seed: &[u8; 32],
    ) {
        self.set_version(PULL_VERSION);
        self.set_bump(bump);
        self.set_status(STATUS_PENDING);
        self.set_count(count);
        self.set_inventory_version(inventory_version);
        self.set_index(index);
        self.set_deadline_slot(deadline_slot);
        self.set_pool(*pool);
        self.set_buyer(*buyer);
        self.set_client_seed(*client_seed);
    }

    #[inline(always)]
    pub fn seeds(pool: &Pubkey, client_seed: &[u8; 32], bump: u8) -> PullSeeds {
        PullSeeds {
            pool: *pool,
            client_seed: *client_seed,
            bump: [bump],
        }
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
    pub fn set_outcome(&mut self, i: usize, tier: u8, asset: &Pubkey) {
        self.outcomes[i][0] = tier;
        self.outcomes[i][1..].copy_from_slice(asset);
    }

    /// The packed `(tier, asset)` entries of the first `count` outcomes, for the event.
    #[inline(always)]
    pub fn outcome_bytes(&self, count: usize) -> &[u8] {
        self.outcomes[..count].as_flattened()
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

const _: () = assert!(core::mem::size_of::<Pool>() == POOL_LEN);
const _: () = assert!(core::mem::size_of::<Item>() == ITEM_LEN);
const _: () = assert!(core::mem::size_of::<Pull>() == PULL_LEN);
const _: () = assert!(core::mem::size_of::<Tier>() == TIER_LEN);
