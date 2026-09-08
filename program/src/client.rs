//! Immutable snapshots and native instructions, with optional read-only RPC.
//! Only a verified proof produces Settlement. Signing stays with the app.

#[cfg(feature = "rpc")]
mod rpc;
#[cfg(feature = "rpc")]
pub use rpc::{Client, RpcClient, RpcError};

use crate::{constants::*, helpers::sha256, state};
use solana_ecvrf::{Proof, PublicKey};
use solana_instruction::{AccountMeta, Instruction};
use solana_message::Message;
use solana_pubkey::Pubkey;
use spl_associated_token_account_interface::{
    address::get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};

pub use crate::errors::GachaError;
pub const PROGRAM_ID: Pubkey = Pubkey::new_from_array(crate::ID);
pub const POOL_HEADER_LEN: usize = POOL_LEN;
const TOKEN: Pubkey = Pubkey::new_from_array(pinocchio_token::ID);
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
/// `tier == None` means this outcome has already been delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub mint: Pubkey,
    pub tier: Option<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Draw {
    pub tier: u8,
    pub position: u32,
    pub item: Pubkey,
}

/// A fresh seed is required for each purchase attempt; never retry stale quotes
/// with an already disclosed seed.
pub struct Buy {
    pub buyer: Pubkey,
    pub count: u8,
    pub client_seed: [u8; 32],
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
    /// Create the vault and pool atomically. Authority pays ATA and pool rent.
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
        let pool = Pool::address_for(authority, self.id);
        let mut data = vec![0];
        for n in [
            self.id,
            self.price,
            self.deadline_slots,
            self.bond_per_draw,
            self.bond,
        ] {
            data.extend(n.to_le_bytes());
        }
        data.push(self.weights.len() as u8);
        for i in 0..MAX_TIERS {
            data.extend(self.weights.get(i).copied().unwrap_or(0).to_le_bytes());
        }
        Ok(vec![
            create_ata(authority, pool, payment_mint),
            instruction(
                data,
                vec![
                    rw(authority, true),
                    rw(pool, false),
                    ro(operator, false),
                    ro(payment_mint, false),
                    rw(ata(pool, payment_mint), false),
                    rw(ata(authority, payment_mint), false),
                    ro(TOKEN, false),
                    ro(SYSTEM, false),
                ],
            ),
        ])
    }
}

macro_rules! number_getters {
    ($($name:ident: $ty:ty),* $(,)?) => { $(pub fn $name(&self) -> $ty { self.header().$name() })* };
}
macro_rules! key_getters {
    ($($name:ident),* $(,)?) => { $(pub fn $name(&self) -> Pubkey { Pubkey::new_from_array(*self.header().$name()) })* };
}

impl Pool {
    pub fn address_for(authority: Pubkey, id: u64) -> Pubkey {
        Pubkey::find_program_address(
            &[POOL_SEED, authority.as_ref(), &id.to_le_bytes()],
            &PROGRAM_ID,
        )
        .0
    }

    /// A 256-byte RPC dataSlice suffices for purchases/admin operations. Settlement
    /// needs the complete account. Decoding does not authenticate the RPC provider.
    pub fn from_account(address: Pubkey, owner: Pubkey, data: &[u8]) -> Result<Self, Error> {
        check(owner, data, POOL_LEN)?;
        let pool = Self {
            address,
            data: data.to_vec(),
        };
        let h = pool.header();
        if !(1..=MAX_TIERS as u8).contains(&h.tier_count())
            || (data.len() != POOL_LEN
                && data.len() != POOL_LEN + crate::inventory::space(h.inventory_version()))
            || Self::address_for(pool.authority(), h.id()) != address
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
        self.data[POOL_LEN + position as usize / 64 * 96 + 32 + position as usize % 64]
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

    /// Use a fresh random seed. Restocking or another purchase may stale the
    /// snapshot; refetch and use a new seed before asking for another signature.
    pub fn buy(&self, buy: Buy) -> Result<Purchase, Error> {
        let Buy {
            buyer,
            count,
            client_seed,
        } = buy;
        if !(1..=MAX_COUNT as u8).contains(&count) {
            return Err(Error::InvalidArgument);
        }
        let mut data = vec![10, count];
        data.extend(client_seed);
        data.extend(self.inventory_version().to_le_bytes());
        let pull = Pull::address_for(self.address, self.next_index());
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

    /// Create custody ATA if needed, then deposit. Submit these together.
    pub fn deposit(&self, tier: u8, mint: Pubkey) -> Result<Vec<Instruction>, Error> {
        if tier >= self.header().tier_count() {
            return Err(Error::InvalidArgument);
        }
        Ok(vec![
            create_ata(self.authority(), self.address, mint),
            instruction(
                vec![1, tier],
                vec![
                    rw(self.authority(), true),
                    rw(self.address, false),
                    rw(
                        Item::address_for(self.address, tier, self.inventory_version()),
                        false,
                    ),
                    ro(mint, false),
                    rw(ata(self.authority(), mint), false),
                    rw(ata(self.address, mint), false),
                    ro(TOKEN, false),
                    ro(SYSTEM, false),
                ],
            ),
        ])
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

    /// The chain checks the current deadline and collateral; no clock is cached.
    /// Prepend native ATA CreateIdempotent if the buyer's payment ATA may be closed.
    pub fn refund(&self, pull: &Pull) -> Result<Instruction, Error> {
        self.pending(pull)?;
        Ok(instruction(
            vec![11],
            vec![
                rw(self.address, false),
                rw(pull.address, false),
                rw(pull.buyer(), false),
                rw(self.vault(), false),
                rw(ata(pull.buyer(), self.payment_mint()), false),
                ro(TOKEN, false),
            ],
        ))
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
        if self.data.len() != POOL_LEN + crate::inventory::space(self.inventory_version()) {
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
            let tier = self
                .header()
                .draw_tier(&hash, &counts)
                .map_err(|_| Error::InvalidAccount)?;
            let list = &mut candidates[tier as usize];
            let rank =
                (u64::from_le_bytes(hash[8..16].try_into().unwrap()) % list.len() as u64) as usize;
            let position = list.remove(rank);
            draws.push(Draw {
                tier,
                position,
                item: Item::address_for(self.address, tier, position),
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
    pub fn address_for(pool: Pubkey, index: u64) -> Pubkey {
        Pubkey::find_program_address(
            &[PULL_SEED, pool.as_ref(), &index.to_le_bytes()],
            &PROGRAM_ID,
        )
        .0
    }
    pub fn from_account(address: Pubkey, owner: Pubkey, data: &[u8]) -> Result<Self, Error> {
        check(owner, data, PULL_LEN)?;
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
        if Self::address_for(pull.pool(), pull.index()) != address {
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
                let (tier, mint) = self.header().outcome(i);
                Outcome {
                    mint: Pubkey::new_from_array(*mint),
                    tier: (tier != DELIVERED).then_some(tier),
                }
            })
            .collect())
    }
    /// Recovery after partial delivery: only undelivered outcomes are included.
    pub fn deliver(&self, payer: Pubkey) -> Result<Vec<Vec<Instruction>>, Error> {
        let pairs = self
            .outcomes()?
            .iter()
            .enumerate()
            .filter(|(_, o)| o.tier.is_some())
            .map(|(i, o)| {
                delivery_pair(
                    self.pool(),
                    self.address,
                    self.buyer(),
                    o.mint,
                    i as u8,
                    payer,
                )
            })
            .collect();
        batches(Vec::new(), pairs, payer)
    }
}

impl Item {
    pub fn address_for(pool: Pubkey, tier: u8, position: u32) -> Pubkey {
        Pubkey::find_program_address(
            &[ITEM_SEED, pool.as_ref(), &[tier], &position.to_le_bytes()],
            &PROGRAM_ID,
        )
        .0
    }
    pub fn from_account(address: Pubkey, owner: Pubkey, data: &[u8]) -> Result<Self, Error> {
        check(owner, data, ITEM_LEN)?;
        if data.len() != ITEM_LEN || data[2] as usize >= MAX_TIERS {
            return Err(Error::InvalidAccount);
        }
        let item = Self {
            address,
            data: data.to_vec(),
        };
        if Self::address_for(item.pool(), item.tier(), item.position()) != address {
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
    key_getters!(pool, mint);
    number_getters!(tier: u8, position: u32);
}

/// Created only by Pool::settle after proof verification. Fetch `draws().item`
/// accounts, decode them as Item, then compose settlement and delivery.
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
    /// Items must correspond to draws in order. Groups fit legacy transactions
    /// with this payer; adding instructions/signers requires another size check.
    pub fn instructions(
        &self,
        items: &[Item],
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
        let pairs = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                delivery_pair(
                    self.pool,
                    self.pull,
                    self.buyer,
                    item.mint(),
                    i as u8,
                    payer,
                )
            })
            .collect();
        batches(vec![self.instruction()], pairs, payer)
    }
}

fn check(owner: Pubkey, data: &[u8], minimum: usize) -> Result<(), Error> {
    if owner != PROGRAM_ID || data.len() < minimum || data[0] != 1 {
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
fn instruction(data: Vec<u8>, accounts: Vec<AccountMeta>) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}
fn deliver(pool: Pubkey, pull: Pubkey, buyer: Pubkey, mint: Pubkey, outcome: u8) -> Instruction {
    instruction(
        vec![21, outcome],
        vec![
            ro(pool, false),
            rw(pull, false),
            rw(buyer, false),
            rw(ata(pool, mint), false),
            rw(ata(buyer, mint), false),
            ro(TOKEN, false),
        ],
    )
}
fn delivery_pair(
    pool: Pubkey,
    pull: Pubkey,
    buyer: Pubkey,
    mint: Pubkey,
    outcome: u8,
    payer: Pubkey,
) -> [Instruction; 2] {
    [
        create_ata(payer, buyer, mint),
        deliver(pool, pull, buyer, mint, outcome),
    ]
}
fn create_ata(payer: Pubkey, owner: Pubkey, mint: Pubkey) -> Instruction {
    create_associated_token_account_idempotent(&payer, &owner, &mint, &TOKEN)
}
fn batches(
    initial: Vec<Instruction>,
    pairs: Vec<[Instruction; 2]>,
    payer: Pubkey,
) -> Result<Vec<Vec<Instruction>>, Error> {
    let mut transactions = vec![initial];
    if transaction_size(&transactions[0], payer) > 1232 {
        return Err(Error::TransactionTooLarge);
    }
    for pair in pairs {
        let current = transactions.last_mut().unwrap();
        current.extend(pair);
        if transaction_size(current, payer) > 1232 {
            let pair = current.split_off(current.len() - 2);
            if transaction_size(&pair, payer) > 1232 {
                return Err(Error::TransactionTooLarge);
            }
            transactions.push(pair);
        }
    }
    transactions.retain(|group| !group.is_empty());
    Ok(transactions)
}
fn transaction_size(instructions: &[Instruction], payer: Pubkey) -> usize {
    let message = Message::new(instructions, Some(&payer));
    1 + 64 * message.header.num_required_signatures as usize + message.serialize().len()
}
