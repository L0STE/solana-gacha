//! Append-only prize positions with mutable availability. A purchase commits
//! to a prefix; deposits beyond that prefix cannot affect any of its draws.
//!
//! Each 96-byte block holds 64 tier tags (zero means removed) and eight Fenwick
//! counts. Prefix counts and rank selection cost O(log(blocks) + 64), without
//! scanning dead prizes, copying snapshots, or passing additional accounts.
//! ponytail: positions are never recycled; the pool grows by 96 bytes per 64
//! deposits. Rotate pools or introduce paged indexes if the 10 MiB account limit
//! becomes relevant (roughly 6.99 million lifetime deposits per pool).

use crate::{constants::MAX_TIERS, errors::GachaError};
use pinocchio::program_error::ProgramError;

pub(crate) use crate::constants::inventory_space as space;
use crate::constants::{
    INVENTORY_BLOCK_LEN as BLOCK_LEN, INVENTORY_COUNTS_LEN as COUNTS_LEN,
    INVENTORY_TAGS_PER_BLOCK as POSITIONS_PER_BLOCK,
};

pub(crate) struct Inventory<'a> {
    data: &'a mut [u8],
}

impl<'a> Inventory<'a> {
    /// The caller validates the pool length against its position count.
    pub(crate) fn new(data: &'a mut [u8]) -> Self {
        Self { data }
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

    pub(crate) fn counts(&self, cutoff: u32) -> [u32; MAX_TIERS] {
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
    pub(crate) fn append(&mut self, position: u32, tier: u8) {
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
    pub(crate) fn take(&mut self, tier: u8, mut rank: u32) -> Result<u32, ProgramError> {
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
    pub(crate) fn remove(&mut self, position: u32, tier: u8) -> Result<(), ProgramError> {
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

/// Append one custody-backed prize at a fresh position. Payer funds item and index rent.
pub(crate) fn restock(
    payer: &pinocchio::account_info::AccountInfo,
    pool_account: &pinocchio::account_info::AccountInfo,
    item_account: &pinocchio::account_info::AccountInfo,
    asset: &pinocchio::pubkey::Pubkey,
    tier: u8,
) -> pinocchio::ProgramResult {
    use crate::{
        constants::*,
        helpers::create_pda,
        state::{Item, Pool},
    };
    use pinocchio::{
        instruction::Seed,
        pubkey::find_program_address,
        sysvars::{rent::Rent, Sysvar},
    };
    let pool = unsafe { Pool::from_bytes_unchecked(pool_account.borrow_data_unchecked()) };
    if pool.status() == POOL_RETIRED {
        return Err(GachaError::InvalidPoolStatus.into());
    }
    if tier >= pool.tier_count() {
        return Err(GachaError::InvalidTier.into());
    }
    let position = pool.inventory_version();
    let next_position = position
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let remaining = pool.tiers()[tier as usize]
        .remaining()
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;

    let tier_seed = [tier];
    let position_seed = position.to_le_bytes();
    let (item_key, bump) = find_program_address(
        &[ITEM_SEED, pool_account.key(), &tier_seed, &position_seed],
        &crate::ID,
    );
    if item_key.ne(item_account.key()) {
        return Err(GachaError::InvalidItem.into());
    }
    let bump_seed = [bump];
    create_pda(
        payer,
        item_account,
        ITEM_LEN,
        &[
            Seed::from(ITEM_SEED),
            Seed::from(pool_account.key()),
            Seed::from(&tier_seed),
            Seed::from(&position_seed),
            Seed::from(&bump_seed),
        ],
    )?;

    let space = POOL_LEN + space(next_position);
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

    let item = unsafe { Item::from_bytes_unchecked_mut(item_account.borrow_mut_data_unchecked()) };
    item.set_version(ITEM_VERSION);
    item.set_bump(bump);
    item.set_tier(tier);
    item.set_position(position);
    item.set_pool(*pool_account.key());
    item.set_asset(*asset);

    let data = unsafe { pool_account.borrow_mut_data_unchecked() };
    let (header, index) = data.split_at_mut(POOL_LEN);
    let pool = unsafe { Pool::from_bytes_unchecked_mut(header) };
    Inventory::new(index).append(position, tier);
    pool.set_inventory_version(next_position);
    let tier = &mut pool.tiers_mut()[tier as usize];
    tier.set_remaining(remaining);
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
