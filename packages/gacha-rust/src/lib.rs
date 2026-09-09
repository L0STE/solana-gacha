//! Immutable snapshots and native instructions, with optional read-only RPC.
//! Only a verified proof produces Settlement. Signing stays with the app.

#[cfg(feature = "rpc")]
mod rpc;
#[cfg(feature = "rpc")]
pub use rpc::{Client, RpcClient, RpcError};

use gacha_core::{constants::*, state};
use solana_ecvrf::{Proof, PublicKey};
use solana_instruction::{AccountMeta, Instruction};
use solana_message::Message;
use solana_pubkey::Pubkey;
use spl_associated_token_account_interface::{
    address::get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};

pub use gacha_core::errors::GachaError;
pub const CORE_PROGRAM_ID: Pubkey = Pubkey::new_from_array(gacha_core::asset::CORE_ID);
pub const PROGRAM_ID: Pubkey = Pubkey::new_from_array(gacha_core::ID);
pub const POOL_HEADER_LEN: usize = POOL_LEN;
/// Every instruction ends with this PDA and the program; events are emitted through them.
pub const EVENT_AUTHORITY: Pubkey = Pubkey::new_from_array(gacha_core::constants::EVENT_AUTHORITY);
const TOKEN: Pubkey = solana_pubkey::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM: Pubkey = Pubkey::new_from_array([0; 32]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidAccount,
    InvalidArgument,
    InvalidProof,
    WrongPool,
    NotPending,
    NotSettled,
    NotNextInQueue,
    MissingInventory,
    InvalidItem,
    InvalidAsset,
    InvalidPoolStatus,
    PendingPurchases,
    TransactionTooLarge,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Owned bytes. `from_account` checks owner, layout, parameters and PDA.
#[derive(Clone, Debug)]
pub struct Pool {
    address: Pubkey,
    data: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct Pull {
    address: Pubkey,
    data: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct Item {
    address: Pubkey,
    data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tier {
    pub weight: u32,
    pub remaining: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Settled,
}
/// Only active pools accept buys and buybacks. Retirement is permanent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PoolStatus {
    Paused = POOL_PAUSED,
    Active = POOL_ACTIVE,
    Retired = POOL_RETIRED,
}
/// `tier == None` means this outcome has already been delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub asset: Pubkey,
    pub tier: Option<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Draw {
    pub tier: u8,
    pub position: u32,
    pub item: Pubkey,
}

/// Core ownership and collection snapshot. Core checks full state and plugin
/// rules at execution; the SDK only reads the transfer header.
#[derive(Clone, Copy, Debug)]
pub struct Asset {
    pub address: Pubkey,
    pub owner: Pubkey,
    pub collection: Option<Pubkey>,
}

impl Asset {
    pub fn from_account(address: Pubkey, owner: Pubkey, data: &[u8]) -> Result<Self, Error> {
        let header = gacha_core::asset::CoreAsset::from_account(&owner.to_bytes(), data)
            .map_err(|_| Error::InvalidAsset)?;
        Ok(Self {
            address,
            owner: Pubkey::new_from_array(*header.owner),
            collection: header.collection.map(|key| Pubkey::new_from_array(*key)),
        })
    }
}

/// A fresh seed is required for each purchase attempt; never retry stale quotes
/// with an already disclosed seed.
pub struct Buy {
    pub buyer: Pubkey,
    pub count: u8,
    pub client_seed: [u8; 32],
}

/// An offer from the pool authority, reusable by any holder until expiry.
#[derive(Clone, Copy, Debug)]
pub struct BuybackQuote {
    pub pool: Pubkey,
    pub asset: Pubkey,
    pub price: u64,
    /// Unix timestamp in seconds. The quoting service sets the five-minute window.
    pub expires_at: i64,
    pub tier: u8,
}

impl BuybackQuote {
    /// Sign these bytes with the pool authority's Ed25519 key.
    pub fn message(&self) -> [u8; 129] {
        gacha_core::buyback_message(
            &self.pool.to_bytes(),
            &self.asset.to_bytes(),
            self.price,
            self.expires_at,
            self.tier,
        )
    }
}

/// Track this address after confirming `instruction`. Preparing is not buying.
#[derive(Debug)]
pub struct Purchase {
    pub pull: Pubkey,
    pub instruction: Instruction,
}

/// Fixed economic parameters; no defaults for price, odds, or penalties.
pub struct CreatePool {
    pub id: u64,
    pub price: u64,
    pub deadline_slots: u64,
    pub bond_per_draw: u64,
    pub bond: u64,
    pub weights: Vec<u32>,
}

impl CreatePool {
    /// Create the vault and paused pool atomically, then fund the vault with
    /// `bond` from the authority's payment ATA. Authority pays ATA and pool rent.
    pub fn instructions(
        &self,
        authority: Pubkey,
        operator: Pubkey,
        payment_mint: Pubkey,
    ) -> Result<Vec<Instruction>, Error> {
        if !(1..=MAX_TIERS).contains(&self.weights.len())
            || self.weights.contains(&0)
            || self.price == 0
            || self.deadline_slots == 0
            || self.bond_per_draw == 0
            || self
                .price
                .checked_add(self.bond_per_draw)
                .and_then(|n| n.checked_mul(MAX_COUNT as u64))
                .is_none()
            || PublicKey(operator.to_bytes()).validate().is_err()
        {
            return Err(Error::InvalidArgument);
        }
        let (pool, bump) = Pool::address_for(authority, self.id);
        let mut data = vec![0];
        for n in [self.id, self.price, self.deadline_slots, self.bond_per_draw] {
            data.extend(n.to_le_bytes());
        }
        data.push(self.weights.len() as u8);
        for i in 0..MAX_TIERS {
            data.extend(self.weights.get(i).copied().unwrap_or(0).to_le_bytes());
        }
        data.push(bump);
        let mut instructions = vec![
            create_ata(authority, pool, payment_mint),
            instruction(
                data,
                vec![
                    rw(authority, true),
                    rw(pool, false),
                    ro(operator, false),
                    ro(payment_mint, false),
                    ro(ata(pool, payment_mint), false),
                    ro(SYSTEM, false),
                ],
            ),
        ];
        if self.bond > 0 {
            instructions.push(token_transfer(
                ata(authority, payment_mint),
                ata(pool, payment_mint),
                authority,
                self.bond,
            ));
        }
        Ok(instructions)
    }
}

macro_rules! number_getters {
    ($($name:ident: $ty:ty),* $(,)?) => { $(pub fn $name(&self) -> $ty { self.header().$name() })* };
}
macro_rules! key_getters {
    ($($name:ident),* $(,)?) => { $(pub fn $name(&self) -> Pubkey { Pubkey::new_from_array(*self.header().$name()) })* };
}

impl Pool {
    /// Address and bump; the bump travels in CreatePool's instruction data.
    pub fn address_for(authority: Pubkey, id: u64) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[POOL_SEED, authority.as_ref(), &id.to_le_bytes()],
            &PROGRAM_ID,
        )
    }

    /// A 256-byte RPC dataSlice suffices for purchases/admin operations. Settlement
    /// needs the complete account. Decoding does not authenticate the RPC provider.
    pub fn from_account(address: Pubkey, owner: Pubkey, data: &[u8]) -> Result<Self, Error> {
        check(owner, data, POOL_LEN, POOL_VERSION)?;
        let pool = Self {
            address,
            data: data.to_vec(),
        };
        let h = pool.header();
        if !(1..=MAX_TIERS as u8).contains(&h.tier_count())
            || h.status() > POOL_RETIRED
            || (data.len() != POOL_LEN
                && data.len() != POOL_LEN + inventory_space(h.inventory_version()))
            || Self::address_for(pool.authority(), h.id()) != (address, h.bump())
            || pool.vault() != ata(address, pool.payment_mint())
            || h.price() == 0
            || h.bond_per_draw() == 0
            || h.deadline_slots() == 0
            || h.refund_amount(MAX_COUNT as u64).is_err()
            || h.tiers().iter().any(|t| t.weight() == 0)
        {
            return Err(Error::InvalidAccount);
        }
        if data.len() > POOL_LEN {
            let mut remaining = [0u32; MAX_TIERS];
            for position in 0..h.inventory_version() {
                let tag = pool.tag(position);
                if tag > h.tier_count() {
                    return Err(Error::InvalidAccount);
                }
                if tag > 0 {
                    remaining[tag as usize - 1] += 1;
                }
            }
            if h.tiers()
                .iter()
                .zip(remaining)
                .any(|(tier, n)| tier.remaining() != n)
            {
                return Err(Error::InvalidAccount);
            }
        }
        Ok(pool)
    }

    fn header(&self) -> &state::Pool {
        unsafe { state::Pool::from_bytes_unchecked(&self.data) }
    }
    fn tag(&self, position: u32) -> u8 {
        self.data[POOL_LEN
            + position as usize / INVENTORY_TAGS_PER_BLOCK * INVENTORY_BLOCK_LEN
            + INVENTORY_COUNTS_LEN
            + position as usize % INVENTORY_TAGS_PER_BLOCK]
    }
    pub fn address(&self) -> Pubkey {
        self.address
    }
    key_getters!(authority, operator, payment_mint, vault);
    number_getters!(id: u64, price: u64, deadline_slots: u64, bond_per_draw: u64, inventory_version: u32, pending_draws: u64, next_index: u64, next_settle: u64);
    pub fn tiers(&self) -> Vec<Tier> {
        self.header()
            .tiers()
            .iter()
            .map(|t| Tier {
                weight: t.weight(),
                remaining: t.remaining(),
            })
            .collect()
    }

    pub fn status(&self) -> PoolStatus {
        match self.header().status() {
            POOL_PAUSED => PoolStatus::Paused,
            POOL_ACTIVE => PoolStatus::Active,
            _ => PoolStatus::Retired,
        }
    }

    /// Authority controls admissions. Retirement is terminal; buyer exits stay open.
    pub fn set_status(&self, status: PoolStatus) -> Result<Instruction, Error> {
        if self.status() == PoolStatus::Retired && status != PoolStatus::Retired {
            return Err(Error::InvalidPoolStatus);
        }
        Ok(instruction(
            vec![3, status as u8],
            vec![ro(self.authority(), true), rw(self.address, false)],
        ))
    }

    /// Surplus after reserving all pending refunds. Supply this pool's current
    /// vault balance in raw payment-token units; execution rechecks the balance.
    pub fn spendable_balance(&self, vault_balance: u64) -> Result<u64, Error> {
        let reserved = self
            .header()
            .refund_amount(self.pending_draws())
            .map_err(|_| Error::InvalidAccount)?;
        Ok(vault_balance.saturating_sub(reserved))
    }

    /// Maximum draws given stock and collateral; zero unless the pool is active.
    /// Buyer funds, token restrictions and changes after this read can still block it.
    pub fn available_draws(&self, vault_balance: u64) -> Result<u8, Error> {
        if self.status() != PoolStatus::Active {
            return Ok(0);
        }
        let stock = self
            .header()
            .remaining()
            .checked_sub(self.pending_draws())
            .ok_or(Error::InvalidAccount)?;
        let funded = self.spendable_balance(vault_balance)? / self.bond_per_draw();
        Ok(stock.min(funded).min(MAX_COUNT as u64) as u8)
    }

    /// Use a fresh random seed. Restocking or another purchase may stale the
    /// snapshot; refetch and use a new seed before asking for another signature.
    pub fn buy(&self, buy: Buy) -> Result<Purchase, Error> {
        if self.status() != PoolStatus::Active {
            return Err(Error::InvalidPoolStatus);
        }
        let Buy {
            buyer,
            count,
            client_seed,
        } = buy;
        if !(1..=MAX_COUNT as u8).contains(&count) {
            return Err(Error::InvalidArgument);
        }
        let (pull, bump) = Pull::address_for(self.address, &client_seed);
        let mut data = vec![10, count];
        data.extend(client_seed);
        data.extend(self.inventory_version().to_le_bytes());
        data.push(bump);
        Ok(Purchase {
            pull,
            instruction: instruction(
                data,
                vec![
                    rw(buyer, true),
                    rw(self.address, false),
                    rw(pull, false),
                    rw(ata(buyer, self.payment_mint()), false),
                    rw(self.vault(), false),
                    ro(TOKEN, false),
                    ro(SYSTEM, false),
                ],
            ),
        })
    }

    /// Deposit an owned Core asset. Collection accounts come from the asset snapshot.
    pub fn deposit(&self, tier: u8, asset: &Asset) -> Result<Instruction, Error> {
        if self.status() == PoolStatus::Retired {
            return Err(Error::InvalidPoolStatus);
        }
        if tier >= self.header().tier_count() {
            return Err(Error::InvalidArgument);
        }
        if asset.owner != self.authority() {
            return Err(Error::InvalidAsset);
        }
        let (item, bump) = Item::address_for(self.address, tier, self.inventory_version());
        Ok(instruction(
            vec![1, tier, bump],
            vec![
                rw(self.authority(), true),
                rw(self.address, false),
                rw(item, false),
                rw(asset.address, false),
                ro(asset.collection.unwrap_or(CORE_PROGRAM_ID), false),
                ro(CORE_PROGRAM_ID, false),
                ro(SYSTEM, false),
            ],
        ))
    }

    /// Return the prize, pay the seller, and restock atomically. Payer sponsors rent.
    /// Refetch the pool and rebuild on an inventory race; the quote remains usable.
    pub fn buyback(
        &self,
        quote: &BuybackQuote,
        signature: &[u8; 64],
        asset: &Asset,
        payer: Pubkey,
    ) -> Result<Vec<Instruction>, Error> {
        if self.status() != PoolStatus::Active {
            return Err(Error::InvalidPoolStatus);
        }
        if quote.pool != self.address {
            return Err(Error::WrongPool);
        }
        if quote.tier >= self.header().tier_count() || quote.price == 0 || quote.expires_at <= 0 {
            return Err(Error::InvalidArgument);
        }
        if quote.asset != asset.address || asset.owner == self.address {
            return Err(Error::InvalidAsset);
        }
        let (item, bump) = Item::address_for(self.address, quote.tier, self.inventory_version());
        let mut data = vec![12, quote.tier];
        data.extend(quote.price.to_le_bytes());
        data.extend(quote.expires_at.to_le_bytes());
        data.extend(signature);
        data.push(bump);
        Ok(vec![
            create_ata(payer, asset.owner, self.payment_mint()),
            instruction(
                data,
                vec![
                    rw(payer, true),
                    ro(asset.owner, true),
                    rw(self.address, false),
                    rw(item, false),
                    rw(asset.address, false),
                    ro(asset.collection.unwrap_or(CORE_PROGRAM_ID), false),
                    rw(self.vault(), false),
                    rw(ata(asset.owner, self.payment_mint()), false),
                    ro(CORE_PROGRAM_ID, false),
                    ro(TOKEN, false),
                    ro(SYSTEM, false),
                ],
            ),
        ])
    }

    /// Return one unsold asset and its Item rent to the authority after retirement.
    /// Pending purchases must settle or refund first; awarded Items cannot be reclaimed.
    pub fn reclaim(&self, item: &Item, asset: &Asset) -> Result<Instruction, Error> {
        if self.status() != PoolStatus::Retired {
            return Err(Error::InvalidPoolStatus);
        }
        if self.pending_draws() != 0 {
            return Err(Error::PendingPurchases);
        }
        if item.pool() != self.address
            || item.tier() >= self.header().tier_count()
            || item.position() >= self.inventory_version()
            || item.asset() != asset.address
        {
            return Err(Error::InvalidItem);
        }
        if asset.owner != self.address {
            return Err(Error::InvalidAsset);
        }
        Ok(instruction(
            vec![4],
            vec![
                rw(self.authority(), true),
                rw(self.address, false),
                rw(item.address(), false),
                rw(asset.address, false),
                ro(asset.collection.unwrap_or(CORE_PROGRAM_ID), false),
                ro(CORE_PROGRAM_ID, false),
                ro(SYSTEM, false),
            ],
        ))
    }

    pub fn withdraw(&self, destination: Pubkey, amount: u64) -> Instruction {
        let mut data = vec![2];
        data.extend(amount.to_le_bytes());
        instruction(
            data,
            vec![
                ro(self.authority(), true),
                ro(self.address, false),
                rw(self.vault(), false),
                rw(destination, false),
                ro(TOKEN, false),
            ],
        )
    }

    /// Recreate the buyer's payment ATA if needed and refund atomically. Only
    /// payer signs; the chain checks the current deadline and collateral.
    pub fn refund(&self, pull: &Pull, payer: Pubkey) -> Result<Vec<Instruction>, Error> {
        self.pending(pull)?;
        Ok(vec![
            create_ata(payer, pull.buyer(), self.payment_mint()),
            instruction(
                vec![11],
                vec![
                    rw(self.address, false),
                    rw(pull.address, false),
                    rw(pull.buyer(), false),
                    rw(self.vault(), false),
                    rw(ata(pull.buyer(), self.payment_mint()), false),
                    ro(TOKEN, false),
                ],
            ),
        ])
    }

    fn pending(&self, pull: &Pull) -> Result<(), Error> {
        if pull.pool() != self.address {
            return Err(Error::WrongPool);
        }
        if pull.status() != Status::Pending {
            return Err(Error::NotPending);
        }
        if pull.index() != self.next_settle() {
            return Err(Error::NotNextInQueue);
        }
        if pull.inventory_version() > self.inventory_version() {
            return Err(Error::InvalidAccount);
        }
        Ok(())
    }

    /// Verify first, then derive the exact item accounts to fetch. Later deposits
    /// cannot affect this plan. The chain rechecks proof, queue and deadline.
    pub fn settle(&self, pull: &Pull, proof: &Proof) -> Result<Settlement, Error> {
        self.pending(pull)?;
        if self.data.len() != POOL_LEN + inventory_space(self.inventory_version()) {
            return Err(Error::MissingInventory);
        }
        let beta = proof
            .verify(&PublicKey(self.operator().to_bytes()), &pull.alpha())
            .map_err(|_| Error::InvalidProof)?;
        // ponytail: host scan is linear in lifetime deposits; use indexed rank
        // queries here too if large pools make client preparation expensive.
        let mut candidates = vec![Vec::new(); self.header().tier_count() as usize];
        for position in 0..pull.inventory_version() {
            let tag = self.tag(position);
            if tag > 0 {
                candidates[tag as usize - 1].push(position);
            }
        }
        let mut draws = Vec::new();
        for i in 0..pull.count() {
            let hash = sha256(&[&beta, &[i]]);
            let mut counts = [0; MAX_TIERS];
            for (t, entries) in candidates.iter().enumerate() {
                counts[t] = entries.len() as u32;
            }
            let (tier, rank) = self
                .header()
                .draw(&hash, &counts)
                .map_err(|_| Error::InvalidAccount)?;
            let position = candidates[tier as usize].remove(rank as usize);
            draws.push(Draw {
                tier,
                position,
                item: Item::address_for(self.address, tier, position).0,
            });
        }
        Ok(Settlement {
            pool: self.address,
            pull: pull.address,
            buyer: pull.buyer(),
            operator: self.operator(),
            proof: *proof,
            draws,
        })
    }
}

impl Pull {
    /// Address and bump; the bump travels in Buy's instruction data. The FIFO
    /// index is assigned on execution and recorded in the account.
    pub fn address_for(pool: Pubkey, client_seed: &[u8; 32]) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[PULL_SEED, pool.as_ref(), client_seed], &PROGRAM_ID)
    }
    pub fn from_account(address: Pubkey, owner: Pubkey, data: &[u8]) -> Result<Self, Error> {
        check(owner, data, PULL_LEN, PULL_VERSION)?;
        if data.len() != PULL_LEN
            || data[2] > STATUS_SETTLED
            || !(1..=MAX_COUNT as u8).contains(&data[3])
        {
            return Err(Error::InvalidAccount);
        }
        let pull = Self {
            address,
            data: data.to_vec(),
        };
        // SDK-built accounts always carry the canonical bump.
        if Self::address_for(pull.pool(), &pull.client_seed()) != (address, pull.header().bump()) {
            return Err(Error::InvalidAccount);
        }
        if pull.status() == Status::Settled
            && (0..pull.count() as usize).any(|i| {
                let tier = pull.header().outcome(i).0;
                tier != DELIVERED && tier as usize >= MAX_TIERS
            })
        {
            return Err(Error::InvalidAccount);
        }
        Ok(pull)
    }
    fn header(&self) -> &state::Pull {
        unsafe { state::Pull::from_bytes_unchecked(&self.data) }
    }
    pub fn address(&self) -> Pubkey {
        self.address
    }
    key_getters!(pool, buyer);
    number_getters!(count: u8, index: u64, inventory_version: u32, deadline_slot: u64);
    pub fn status(&self) -> Status {
        if self.header().status() == STATUS_PENDING {
            Status::Pending
        } else {
            Status::Settled
        }
    }
    pub fn client_seed(&self) -> [u8; 32] {
        *self.header().client_seed()
    }
    pub fn alpha(&self) -> [u8; 32] {
        sha256(&[self.address.as_ref(), self.header().client_seed()])
    }
    pub fn outcomes(&self) -> Result<Vec<Outcome>, Error> {
        if self.status() != Status::Settled {
            return Err(Error::NotSettled);
        }
        Ok((0..self.count() as usize)
            .map(|i| {
                let (tier, asset) = self.header().outcome(i);
                Outcome {
                    asset: Pubkey::new_from_array(*asset),
                    tier: (tier != DELIVERED).then_some(tier),
                }
            })
            .collect())
    }
    /// Recovery after partial delivery: only undelivered outcomes are included.
    pub fn deliver(&self, assets: &[Asset], payer: Pubkey) -> Result<Vec<Vec<Instruction>>, Error> {
        let outcomes = self.outcomes()?;
        let pending: Vec<_> = outcomes
            .iter()
            .enumerate()
            .filter(|(_, o)| o.tier.is_some())
            .collect();
        if pending.len() != assets.len() {
            return Err(Error::InvalidAsset);
        }
        let instructions = pending
            .iter()
            .zip(assets)
            .map(|((i, outcome), asset)| {
                if outcome.asset != asset.address {
                    return Err(Error::InvalidAsset);
                }
                deliver(
                    self.pool(),
                    self.address,
                    self.buyer(),
                    asset,
                    *i as u8,
                    payer,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        batches(Vec::new(), instructions, payer)
    }
}

impl Item {
    /// Address and bump; the bump travels in DepositItem's and Buyback's instruction data.
    pub fn address_for(pool: Pubkey, tier: u8, position: u32) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[ITEM_SEED, pool.as_ref(), &[tier], &position.to_le_bytes()],
            &PROGRAM_ID,
        )
    }
    pub fn from_account(address: Pubkey, owner: Pubkey, data: &[u8]) -> Result<Self, Error> {
        check(owner, data, ITEM_LEN, ITEM_VERSION)?;
        if data.len() != ITEM_LEN || data[2] as usize >= MAX_TIERS {
            return Err(Error::InvalidAccount);
        }
        let item = Self {
            address,
            data: data.to_vec(),
        };
        if Self::address_for(item.pool(), item.tier(), item.position())
            != (address, item.header().bump())
        {
            return Err(Error::InvalidAccount);
        }
        Ok(item)
    }
    fn header(&self) -> &state::Item {
        unsafe { state::Item::from_bytes_unchecked(&self.data) }
    }
    pub fn address(&self) -> Pubkey {
        self.address
    }
    key_getters!(pool, asset);
    number_getters!(tier: u8, position: u32);
}

/// Created only by Pool::settle after proof verification. Fetch `draws().item`
/// accounts, decode them as Item, fetch their Asset headers, then compose delivery.
#[derive(Clone, Debug)]
pub struct Settlement {
    pool: Pubkey,
    pull: Pubkey,
    buyer: Pubkey,
    operator: Pubkey,
    proof: Proof,
    draws: Vec<Draw>,
}
impl Settlement {
    pub fn draws(&self) -> &[Draw] {
        &self.draws
    }
    pub fn instruction(&self) -> Instruction {
        let mut data = vec![20];
        data.extend(self.proof.0);
        let mut accounts = vec![
            rw(self.operator, false),
            rw(self.pool, false),
            rw(self.pull, false),
        ];
        accounts.extend(self.draws.iter().map(|d| rw(d.item, false)));
        instruction(data, accounts)
    }
    /// Items and assets must correspond to draws in order. Groups fit legacy
    /// transactions with this payer; additions require another size check.
    pub fn instructions(
        &self,
        items: &[Item],
        assets: &[Asset],
        payer: Pubkey,
    ) -> Result<Vec<Vec<Instruction>>, Error> {
        if items.len() != self.draws.len()
            || items
                .iter()
                .zip(&self.draws)
                .any(|(item, d)| item.address != d.item)
        {
            return Err(Error::InvalidItem);
        }
        if assets.len() != items.len() {
            return Err(Error::InvalidAsset);
        }
        let instructions = items
            .iter()
            .zip(assets)
            .enumerate()
            .map(|(i, (item, asset))| {
                if item.asset() != asset.address {
                    return Err(Error::InvalidAsset);
                }
                deliver(self.pool, self.pull, self.buyer, asset, i as u8, payer)
            })
            .collect::<Result<Vec<_>, _>>()?;
        batches(vec![self.instruction()], instructions, payer)
    }
}

fn check(owner: Pubkey, data: &[u8], minimum: usize, version: u8) -> Result<(), Error> {
    if owner != PROGRAM_ID || data.len() < minimum || data[0] != version {
        return Err(Error::InvalidAccount);
    }
    Ok(())
}
fn ata(owner: Pubkey, mint: Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(&owner, &mint, &TOKEN)
}
fn rw(key: Pubkey, signer: bool) -> AccountMeta {
    AccountMeta::new(key, signer)
}
fn ro(key: Pubkey, signer: bool) -> AccountMeta {
    AccountMeta::new_readonly(key, signer)
}
/// Every instruction ends with the event authority and the program, for the event CPI.
fn instruction(data: Vec<u8>, mut accounts: Vec<AccountMeta>) -> Instruction {
    accounts.push(ro(EVENT_AUTHORITY, false));
    accounts.push(ro(PROGRAM_ID, false));
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}
fn deliver(
    pool: Pubkey,
    pull: Pubkey,
    buyer: Pubkey,
    asset: &Asset,
    outcome: u8,
    payer: Pubkey,
) -> Result<Instruction, Error> {
    if asset.owner != pool {
        return Err(Error::InvalidAsset);
    }
    Ok(instruction(
        vec![21, outcome],
        vec![
            rw(payer, true),
            ro(pool, false),
            rw(pull, false),
            rw(buyer, false),
            rw(asset.address, false),
            ro(asset.collection.unwrap_or(CORE_PROGRAM_ID), false),
            ro(CORE_PROGRAM_ID, false),
            ro(SYSTEM, false),
        ],
    ))
}
/// SPL Token `Transfer`; collateral enters the vault as an ordinary transfer.
fn token_transfer(from: Pubkey, to: Pubkey, authority: Pubkey, amount: u64) -> Instruction {
    let mut data = vec![3];
    data.extend(amount.to_le_bytes());
    Instruction {
        program_id: TOKEN,
        accounts: vec![rw(from, false), rw(to, false), ro(authority, true)],
        data,
    }
}
fn create_ata(payer: Pubkey, owner: Pubkey, mint: Pubkey) -> Instruction {
    create_associated_token_account_idempotent(&payer, &owner, &mint, &TOKEN)
}
fn batches(
    initial: Vec<Instruction>,
    instructions: Vec<Instruction>,
    payer: Pubkey,
) -> Result<Vec<Vec<Instruction>>, Error> {
    let mut transactions = vec![initial];
    if transaction_size(&transactions[0], payer) > 1232 {
        return Err(Error::TransactionTooLarge);
    }
    for instruction in instructions {
        let current = transactions.last_mut().unwrap();
        current.push(instruction);
        if transaction_size(current, payer) > 1232 {
            let next = current.split_off(current.len() - 1);
            if transaction_size(&next, payer) > 1232 {
                return Err(Error::TransactionTooLarge);
            }
            transactions.push(next);
        }
    }
    transactions.retain(|group| !group.is_empty());
    Ok(transactions)
}
fn transaction_size(instructions: &[Instruction], payer: Pubkey) -> usize {
    let message = Message::new(instructions, Some(&payer));
    1 + 64 * message.header.num_required_signatures as usize + message.serialize().len()
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    for part in parts {
        hash.update(part);
    }
    hash.finalize().into()
}
