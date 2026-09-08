pub const POOL_VERSION: u8 = 1;
pub const ITEM_VERSION: u8 = 1;
pub const PULL_VERSION: u8 = 1;

pub const POOL_PAUSED: u8 = 0;
pub const POOL_ACTIVE: u8 = 1;
pub const POOL_RETIRED: u8 = 2;

/// Pool PDA: [POOL_SEED, authority, id (u64 LE)].
pub const POOL_SEED: &[u8] = b"pool";
/// Item PDA: [ITEM_SEED, pool, tier (u8), global deposit position (u32 LE)].
pub const ITEM_SEED: &[u8] = b"item";
/// Pull PDA: [PULL_SEED, pool, index (u64 LE)].
pub const PULL_SEED: &[u8] = b"pull";

pub const MAX_TIERS: usize = 8;
/// Draws per pull. Bounds the outcome table in `Pull` and the item accounts a
/// settle carries (one per draw).
pub const MAX_COUNT: usize = 10;

/// Fixed header length; the pool also has a variable availability-index suffix.
pub const POOL_LEN: usize = 192 + MAX_TIERS * TIER_LEN; // 256
pub const TIER_LEN: usize = 8;
pub const ITEM_LEN: usize = 72;
pub const PULL_LEN: usize = 120 + MAX_COUNT * OUTCOME_LEN; // 450
pub const OUTCOME_LEN: usize = 33;
pub const OUTCOMES_OFFSET: usize = 120;

pub const STATUS_PENDING: u8 = 0;
pub const STATUS_SETTLED: u8 = 1;
/// An outcome's tier byte once its item has been delivered.
pub const DELIVERED: u8 = 0xff;

/// Inventory suffix: Fenwick counts followed by tier tags for each block.
pub const INVENTORY_TAGS_PER_BLOCK: usize = 64;
pub const INVENTORY_COUNTS_LEN: usize = MAX_TIERS * 4;
pub const INVENTORY_BLOCK_LEN: usize = INVENTORY_COUNTS_LEN + INVENTORY_TAGS_PER_BLOCK;

#[inline]
pub const fn inventory_space(positions: u32) -> usize {
    (positions as usize).div_ceil(INVENTORY_TAGS_PER_BLOCK) * INVENTORY_BLOCK_LEN
}
