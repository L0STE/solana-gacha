//! Append-only prize positions with mutable availability. A purchase commits
//! to a prefix; deposits beyond that prefix cannot affect any of its draws.
//!
//! Each 96-byte block holds 64 tier tags (zero means removed) and eight Fenwick
//! counts. Prefix counts and rank selection cost O(log(blocks) + 64), without
//! scanning dead prizes, copying snapshots, or passing additional accounts.
//! ponytail: positions are never recycled; the pool grows by 96 bytes per 64
//! deposits. Rotate pools or introduce paged indexes if the 10 MiB account limit
//! becomes relevant (roughly 6.99 million lifetime deposits per pool).

use crate::{
    constants::{MAX_TIERS, POOL_LEN},
    errors::GachaError,
    state::{Item, Load, Pool, Pull},
};
use pinocchio::{
    account_info::AccountInfo, program_error::ProgramError, pubkey::Pubkey, ProgramResult,
};

pub use crate::constants::inventory_space as space;
use crate::constants::{
    INVENTORY_BLOCK_LEN as BLOCK_LEN, INVENTORY_COUNTS_LEN as COUNTS_LEN,
    INVENTORY_TAGS_PER_BLOCK as POSITIONS_PER_BLOCK,
};

pub struct Inventory<'a> {
    data: &'a mut [u8],
}

impl<'a> Inventory<'a> {
    /// The caller validates the pool length against its position count.
    pub fn new(data: &'a mut [u8]) -> Self {
        Self { data }
    }

    /// View the pool's availability index after validating the pool. It lives
    /// past the fixed header, so it can be held alongside `Pool::load_mut`.
    #[inline(always)]
    pub fn load(pool_account: &'a AccountInfo) -> Result<Self, ProgramError> {
        Pool::load(pool_account)?;
        // SAFETY: validated just above; the index bytes never overlap the header
        // a `Pool` view covers.
        let data = unsafe { pool_account.borrow_mut_data_unchecked() };
        Ok(Self::new(&mut data[POOL_LEN..]))
    }

    fn blocks(&self) -> usize {
        self.data.len() / BLOCK_LEN
    }

    // Fenwick nodes are 1-based; physical blocks and prize positions are 0-based.
    fn count(&self, node: usize, tier: usize) -> u32 {
        let start = (node - 1) * BLOCK_LEN + tier * 4;
        u32::from_le_bytes(self.data[start..start + 4].try_into().unwrap())
    }

    fn set_count(&mut self, node: usize, tier: usize, count: u32) {
        let start = (node - 1) * BLOCK_LEN + tier * 4;
        self.data[start..start + 4].copy_from_slice(&count.to_le_bytes());
    }

    // Node-outer, tier-inner: walking the Fenwick chain once per tier instead
    // measured 3,400 CU worse on a ten-draw settle; the 32-byte zero fill is cheaper.
    fn prefix_counts(&self, mut blocks: usize) -> [u32; MAX_TIERS] {
        let mut counts = [0; MAX_TIERS];
        while blocks > 0 {
            for (tier, count) in counts.iter_mut().enumerate() {
                *count += self.count(blocks, tier);
            }
            blocks &= blocks - 1;
        }
        counts
    }

    pub fn counts(&self, cutoff: u32) -> [u32; MAX_TIERS] {
        let blocks = cutoff as usize / POSITIONS_PER_BLOCK;
        let tail = cutoff as usize % POSITIONS_PER_BLOCK;
        let mut counts = self.prefix_counts(blocks);
        let start = blocks * BLOCK_LEN + COUNTS_LEN;
        if tail > 0 {
            for &tag in &self.data[start..start + tail] {
                if tag != 0 {
                    counts[tag as usize - 1] += 1;
                }
            }
        }
        counts
    }

    /// `position` is the pool's next never-reused index. Any new block has
    /// already been zero-extended by DepositItem.
    pub fn append(&mut self, position: u32, tier: u8) {
        let block = position as usize / POSITIONS_PER_BLOCK;
        let offset = position as usize % POSITIONS_PER_BLOCK;
        let mut node = block + 1;
        if offset == 0 {
            // A newly appended Fenwick node also covers some older blocks.
            let end = self.prefix_counts(block);
            let start = self.prefix_counts(node & (node - 1));
            for tier in 0..MAX_TIERS {
                self.set_count(node, tier, end[tier] - start[tier]);
            }
        }
        self.data[block * BLOCK_LEN + COUNTS_LEN + offset] = tier + 1;
        while node <= self.blocks() {
            self.set_count(node, tier as usize, self.count(node, tier as usize) + 1);
            node += node & node.wrapping_neg();
        }
    }

    /// Remove the zero-based `rank`th available prize in `tier`. The caller
    /// bounds rank by counts(cutoff), so the selected position is < cutoff.
    pub fn take(&mut self, tier: u8, mut rank: u32) -> Result<u32, ProgramError> {
        let blocks = self.blocks();
        let mut node = 0;
        // Skip whole blocks whose available prizes precede the requested rank.
        let mut step = blocks.next_power_of_two();
        while step != 0 {
            let next = node + step;
            if next <= blocks {
                let count = self.count(next, tier as usize);
                if count <= rank {
                    rank -= count;
                    node = next;
                } else {
                    // This node contains the selected prize; update it during the search.
                    self.set_count(next, tier as usize, count - 1);
                }
            }
            step >>= 1;
        }
        if node == blocks {
            return Err(GachaError::InvalidItem.into());
        }
        // The remaining rank is local to this block's tier tags.
        let start = node * BLOCK_LEN + COUNTS_LEN;
        let tags = &mut self.data[start..start + POSITIONS_PER_BLOCK];
        for (offset, tag) in tags.iter_mut().enumerate() {
            if *tag != tier + 1 {
                continue;
            }
            if rank != 0 {
                rank -= 1;
                continue;
            }
            *tag = 0;
            let position = (node * POSITIONS_PER_BLOCK + offset) as u32;
            return Ok(position);
        }
        Err(GachaError::InvalidItem.into())
    }

    /// Remove one known unsold position during retirement.
    pub fn remove(&mut self, position: u32, tier: u8) -> Result<(), ProgramError> {
        let block = position as usize / POSITIONS_PER_BLOCK;
        let offset = position as usize % POSITIONS_PER_BLOCK;
        let tag = self
            .data
            .get_mut(block * BLOCK_LEN + COUNTS_LEN + offset)
            .filter(|tag| **tag == tier + 1)
            .ok_or(GachaError::InvalidItem)?;
        *tag = 0;
        let mut node = block + 1;
        while node <= self.blocks() {
            let count = self
                .count(node, tier as usize)
                .checked_sub(1)
                .ok_or(GachaError::InvalidItem)?;
            self.set_count(node, tier as usize, count);
            node += node & node.wrapping_neg();
        }
        Ok(())
    }
}

/// Settle every draw of `pull` from the verified VRF output: pick a tier by
/// weight and an item by rank within the pull's pinned prefix, record the
/// award, and close the Item to the operator. Each passed Item must be the one
/// its draw selected.
pub fn draw(
    pool: &mut Pool,
    mut inventory: Inventory,
    pull: &mut Pull,
    pool_key: &Pubkey,
    items: &[AccountInfo],
    operator: &AccountInfo,
    beta: &[u8; 64],
) -> ProgramResult {
    use crate::helpers::{close, sha256};
    // Only our own draws reduce the eligible stock during this instruction
    let mut available = inventory.counts(pull.inventory_version());
    for (i, item_account) in items.iter().enumerate() {
        let hash = sha256(&[beta, &[i as u8]]);
        let (tier, rank) = pool.draw(&hash, &available)?;
        let position = inventory.take(tier, rank)?;
        available[tier as usize] -= 1;

        let item = Item::load(item_account)?;
        if item.pool().ne(pool_key) || item.tier().ne(&tier) || item.position().ne(&position) {
            return Err(GachaError::InvalidItem.into());
        }

        pull.set_outcome(i, tier, item.asset());
        close(item_account, operator)?;
        let selected = &mut pool.tiers_mut()[tier as usize];
        selected.set_remaining(selected.remaining() - 1);
    }
    Ok(())
}

/// The pool accepts stock in `tier` and `item_account` is the empty PDA for
/// the pool's next position. Returns the position.
/// Positions are u32: the account-size ceiling is reached long before overflow.
pub fn check_restock(
    pool: &Pool,
    pool_key: &Pubkey,
    item_account: &AccountInfo,
    tier: u8,
    bump: u8,
) -> Result<u32, ProgramError> {
    use crate::{constants::ITEM_SEED, helpers::check_uninitialized};
    use pinocchio::pubkey::create_program_address;
    if tier >= pool.tier_count() {
        return Err(GachaError::InvalidTier.into());
    }
    let position = pool.inventory_version();
    let key = create_program_address(
        &[
            ITEM_SEED,
            pool_key,
            &[tier],
            &position.to_le_bytes(),
            &[bump],
        ],
        &crate::ID,
    )
    .map_err(|_| GachaError::InvalidSeeds)?;
    if key.ne(item_account.key()) {
        return Err(GachaError::InvalidSeeds.into());
    }
    check_uninitialized(item_account)?;
    Ok(position)
}

/// Append one custody-backed prize at the pool's next position. Payer funds
/// item and index rent. Callers run `check_restock` first.
pub fn restock(
    payer: &AccountInfo,
    pool_account: &AccountInfo,
    item_account: &AccountInfo,
    asset: &Pubkey,
    tier: u8,
    bump: u8,
) -> ProgramResult {
    use crate::{constants::ITEM_LEN, helpers::create_pda};
    use pinocchio::sysvars::{rent::Rent, Sysvar};
    // Take the next position and grow the index first, so the pool's length and
    // version stay consistent for every view taken below.
    let pool = Pool::load_mut(pool_account)?;
    let position = pool.inventory_version();
    pool.set_inventory_version(position + 1);
    let space = POOL_LEN + space(position + 1);
    if space != pool_account.data_len() {
        let missing = Rent::get()?
            .minimum_balance(space)
            .saturating_sub(pool_account.lamports());
        if missing > 0 {
            pinocchio_system::instructions::Transfer {
                from: payer,
                to: pool_account,
                lamports: missing,
            }
            .invoke()?;
        }
        pool_account.realloc(space, false)?;
    }

    // Create and populate the Item
    let seeds = Item::seeds(pool_account.key(), tier, position, bump);
    create_pda(payer, item_account, ITEM_LEN, &seeds.as_seeds())?;
    let item = Item::load_new(item_account)?;
    item.set_inner(bump, tier, position, pool_account.key(), asset);

    // Make the position available
    let pool = Pool::load_mut(pool_account)?;
    let mut inventory = Inventory::load(pool_account)?;
    inventory.append(position, tier);
    let tier = &mut pool.tiers_mut()[tier as usize];
    tier.set_remaining(tier.remaining() + 1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growing_index_matches_a_linear_inventory_through_depletion_and_restocking() {
        let mut data = Vec::new();
        let mut reference = Vec::new();
        let mut random = 12345u64;
        for position in 0..2049u32 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let tier = (random >> 32) as u8 % MAX_TIERS as u8;
            data.resize(space(position + 1), 0);
            let mut index = Inventory::new(&mut data);
            index.append(position, tier);
            reference.push(tier + 1);
            for cutoff in [0, 1, 63, 64, 65, 127, 128, 129, position, position + 1] {
                if cutoff > position + 1 {
                    continue;
                }
                let mut counts = [0u32; MAX_TIERS];
                for &tag in &reference[..cutoff as usize] {
                    if tag > 0 {
                        counts[tag as usize - 1] += 1;
                    }
                }
                assert_eq!(
                    index.counts(cutoff),
                    counts,
                    "cutoff {cutoff}, length {}",
                    position + 1
                );
            }
            if position % 3 != 0 {
                let cutoff = (random % (position as u64 + 1) + 1) as u32;
                let counts = index.counts(cutoff);
                for (tier, &count) in counts.iter().enumerate() {
                    if count == 0 {
                        continue;
                    }
                    let rank = (random % count as u64) as u32;
                    let expected = reference[..cutoff as usize]
                        .iter()
                        .enumerate()
                        .filter(|(_, tag)| **tag == tier as u8 + 1)
                        .nth(rank as usize)
                        .unwrap()
                        .0;
                    assert_eq!(index.take(tier as u8, rank).unwrap(), expected as u32);
                    reference[expected] = 0;
                }
            }
        }
        let mut index = Inventory::new(&mut data);
        // Retirement removes known positions in arbitrary order, using the same index.
        for (position, tag) in reference.iter_mut().enumerate().rev().step_by(2) {
            if *tag > 0 {
                index.remove(position as u32, *tag - 1).unwrap();
                assert!(index.remove(position as u32, *tag - 1).is_err());
                *tag = 0;
            }
        }
        for (position, &tag) in reference.iter().enumerate() {
            if tag > 0 {
                assert_eq!(index.take(tag - 1, 0).unwrap(), position as u32);
            }
        }
        assert_eq!(index.counts(reference.len() as u32), [0; MAX_TIERS]);
        assert!(index.take(0, 0).is_err());
    }
}
