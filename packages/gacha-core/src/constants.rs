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
/// Pull PDA: [PULL_SEED, pool, client_seed]. The FIFO index is assigned on
/// execution, so concurrent purchases never contend for one address.
pub const PULL_SEED: &[u8] = b"pull";
/// Event authority PDA: [EVENT_AUTHORITY_SEED]. Every instruction emits one
/// event by invoking the program itself with this PDA as signer, so events
/// live in inner instructions and cannot be spoofed by other programs.
pub const EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";
// DzGCFfQ4o9bvpN3mhNmibxnf52DxBh7m7Ym8mbNqfpea; the test below re-derives it.
pub const EVENT_AUTHORITY: [u8; 32] = [
    192, 247, 135, 119, 0, 6, 222, 140, 33, 33, 148, 202, 138, 73, 98, 34, 75, 178, 173, 186, 91,
    23, 18, 107, 166, 29, 116, 219, 149, 151, 122, 175,
];
pub const EVENT_AUTHORITY_BUMP: u8 = 254;
/// Self-CPI event instruction discriminator.
pub const EVENT_DISCRIMINATOR: u8 = 255;

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
