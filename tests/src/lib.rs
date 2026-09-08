//! Test fixture: a Mollusk instance with the gacha, Core and token programs, and
//! builders for every instruction. The `Model` mirrors the on-chain draw so a
//! test can know which item accounts a settle needs, exactly as an operator
//! backend does.

use gacha_core::constants::*;
use mollusk_svm::{program::keyed_account_for_system_program, result::InstructionResult, Mollusk};
use solana_account::Account;
use solana_ecvrf::{PublicKey, SecretKey};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

pub const PROGRAM_ID: Pubkey = Pubkey::new_from_array(gacha_core::ID);
pub const CORE: Pubkey = gacha_rust::CORE_PROGRAM_ID;
pub const TOKEN: Pubkey = mollusk_svm_programs_token::token::ID;
pub const ATA_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM: Pubkey = Pubkey::new_from_array([0; 32]);

pub const AUTHORITY_SEED: [u8; 32] = [7; 32];

pub const PRICE: u64 = 5_000_000;
pub const BOND_PER_DRAW: u64 = 1_000_000;
pub const BOND: u64 = 50_000_000;
pub const DEADLINE_SLOTS: u64 = 100;
pub const WEIGHTS: [u32; 3] = [79, 20, 1];

pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

pub fn pool_pda(authority: &Pubkey, id: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[POOL_SEED, authority.as_ref(), &id.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

pub fn item_pda(pool: &Pubkey, tier: u8, position: u32) -> Pubkey {
    Pubkey::find_program_address(
        &[ITEM_SEED, pool.as_ref(), &[tier], &position.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

pub fn pull_pda(pool: &Pubkey, index: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[PULL_SEED, pool.as_ref(), &index.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

/// An SPL token account, packed by hand (165 bytes).
pub fn token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Account {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // initialized
    Account {
        lamports: 2_039_280,
        data,
        owner: TOKEN,
        executable: false,
        rent_epoch: 0,
    }
}

/// An SPL mint (82 bytes), supply `supply`, decimals 0, no authorities.
pub fn mint_account(supply: u64) -> Account {
    let mut data = vec![0u8; 82];
    data[36..44].copy_from_slice(&supply.to_le_bytes());
    data[45] = 1; // initialized
    Account {
        lamports: 1_461_600,
        data,
        owner: TOKEN,
        executable: false,
        rent_epoch: 0,
    }
}

/// A complete uncompressed Core AssetV1 with empty metadata and no plugins.
pub fn core_asset(owner: &Pubkey, collection: Option<&Pubkey>) -> Account {
    let mut data = vec![1];
    data.extend(owner.as_ref());
    data.push(if collection.is_some() { 2 } else { 0 });
    if let Some(collection) = collection {
        data.extend(collection.as_ref());
    }
    data.extend([0; 9]); // empty name, empty URI, no sequence number
    Account {
        lamports: 2_000_000,
        data,
        owner: CORE,
        executable: false,
        rent_epoch: 0,
    }
}

pub fn wallet(lamports: u64) -> Account {
    Account {
        lamports,
        data: vec![],
        owner: SYSTEM,
        executable: false,
        rent_epoch: 0,
    }
}

pub fn token_amount(account: &Account) -> u64 {
    u64::from_le_bytes(account.data[64..72].try_into().unwrap())
}

/// Off-chain mirror of the pool's tier lists, so a test can pass the item
/// accounts a settle will draw. The operator backend does the same.
#[derive(Clone)]
pub struct Model {
    pub weights: Vec<u32>,
    /// Immutable global deposit positions; awarded entries become None.
    pub items: Vec<Option<(u8, Pubkey)>>,
}

impl Model {
    pub fn draw_tier(&self, hash: &[u8; 32], cutoff: usize) -> Option<u8> {
        let mut counts = vec![0; self.weights.len()];
        for &(tier, _) in self.items[..cutoff].iter().flatten() {
            counts[tier as usize] += 1;
        }
        let total: u64 = counts
            .iter()
            .zip(&self.weights)
            .filter(|(count, _)| **count > 0)
            .map(|(_, &w)| w as u64)
            .sum();
        if total == 0 {
            return None;
        }
        let mut roll = u64::from_le_bytes(hash[..8].try_into().unwrap()) % total;
        for (i, (&count, &w)) in counts.iter().zip(&self.weights).enumerate() {
            if count == 0 {
                continue;
            }
            if roll < w as u64 {
                return Some(i as u8);
            }
            roll -= w as u64;
        }
        unreachable!()
    }

    /// Independent linear reference: filter the signed prefix, choose by rank.
    pub fn draw(&mut self, hash: &[u8; 32], cutoff: usize) -> Option<(u8, u32, Pubkey)> {
        let tier = self.draw_tier(hash, cutoff)?;
        let candidates: Vec<_> = self.items[..cutoff]
            .iter()
            .enumerate()
            .filter_map(|(position, item)| {
                item.filter(|(t, _)| *t == tier)
                    .map(|(_, asset)| (position, asset))
            })
            .collect();
        let rank = (u64::from_le_bytes(hash[8..16].try_into().unwrap()) % candidates.len() as u64)
            as usize;
        let (position, asset) = candidates[rank];
        self.items[position] = None;
        Some((tier, position as u32, asset))
    }
}

pub struct Fixture {
    pub mollusk: Mollusk,
    pub authority: Pubkey,
    pub operator_key: SecretKey,
    pub operator: Pubkey,
    pub buyer: Pubkey,
    pub payment_mint: Pubkey,
    pub pool: Pubkey,
    pub vault: Pubkey,
    pub accounts: Vec<(Pubkey, Account)>,
    pub model: Model,
    pub next_index: u64,
}

impl Fixture {
    pub fn new() -> Self {
        std::env::set_var(
            "SBF_OUT_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy"),
        );
        let mut mollusk = Mollusk::new(&PROGRAM_ID, "gacha_program");
        mollusk.add_program(&CORE, "mpl_core_program");
        mollusk_svm_programs_token::token::add_program(&mut mollusk);
        mollusk_svm_programs_token::associated_token::add_program(&mut mollusk);

        let authority = Pubkey::new_from_array(
            ed25519_dalek::SigningKey::from_bytes(&AUTHORITY_SEED)
                .verifying_key()
                .to_bytes(),
        );
        let operator_key = SecretKey([9u8; 32]);
        let operator = Pubkey::new_from_array(operator_key.public_key().0);
        let buyer = Pubkey::new_unique();
        let payment_mint = Pubkey::new_unique();
        let pool = pool_pda(&authority, 1);
        let vault = ata(&pool, &payment_mint);

        let accounts = vec![
            (authority, wallet(10_000_000_000)),
            (operator, wallet(10_000_000_000)),
            (buyer, wallet(10_000_000_000)),
            (payment_mint, mint_account(1_000_000_000)),
            (vault, token_account(&payment_mint, &pool, 0)),
            (
                ata(&authority, &payment_mint),
                token_account(&payment_mint, &authority, BOND),
            ),
            (
                ata(&buyer, &payment_mint),
                token_account(&payment_mint, &buyer, 1_000_000_000),
            ),
            (pool, wallet(0)),
            keyed_account_for_system_program(),
            (
                CORE,
                mollusk_svm::program::create_program_account_loader_v3(&CORE),
            ),
            mollusk_svm_programs_token::token::keyed_account(),
            mollusk_svm_programs_token::associated_token::keyed_account(),
        ];

        Self {
            mollusk,
            authority,
            operator_key,
            operator,
            buyer,
            payment_mint,
            pool,
            vault,
            accounts,
            model: Model {
                weights: WEIGHTS.to_vec(),
                items: vec![],
            },
            next_index: 0,
        }
    }

    pub fn account(&self, key: &Pubkey) -> &Account {
        &self
            .accounts
            .iter()
            .find(|(k, _)| k == key)
            .expect("account")
            .1
    }

    pub fn upsert(&mut self, key: Pubkey, account: Account) {
        match self.accounts.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = account,
            None => self.accounts.push((key, account)),
        }
    }

    /// Make sure every account the instruction names exists, as an empty
    /// wallet if nothing created it yet (PDAs about to be created).
    pub fn ensure(&mut self, ix: &Instruction) {
        for meta in &ix.accounts {
            if !self.accounts.iter().any(|(k, _)| *k == meta.pubkey) {
                self.accounts.push((meta.pubkey, wallet(0)));
            }
        }
    }

    /// Fold successful instructions only. Failed fixture state is not rollback evidence;
    /// assert against run_transaction's returned accounts to test atomicity.
    pub fn run(&mut self, ix: &Instruction) -> InstructionResult {
        self.ensure(ix);
        let result = self.mollusk.process_instruction(ix, &self.accounts);
        if result.program_result.is_ok() {
            for (key, account) in &result.resulting_accounts {
                self.upsert(*key, account.clone());
            }
        }
        result
    }

    /// Execute one complete transaction, including atomic rollback on failure.
    pub fn run_transaction(
        &mut self,
        instructions: &[Instruction],
        payer: &Pubkey,
    ) -> mollusk_svm::result::types::TransactionResult {
        for ix in instructions {
            self.ensure(ix);
        }
        let result = self.mollusk.process_transaction_instructions(
            instructions,
            &self.accounts,
            Some(payer),
        );
        if result.raw_result.is_ok() {
            for (key, account) in &result.resulting_accounts {
                self.upsert(*key, account.clone());
            }
        }
        result
    }

    pub fn pool_state(&self) -> Vec<u8> {
        self.account(&self.pool).data.clone()
    }

    pub fn client_pool(&self) -> gacha_rust::Pool {
        let account = self.account(&self.pool);
        gacha_rust::Pool::from_account(self.pool, account.owner, &account.data).unwrap()
    }

    pub fn client_pull(&self, pull: &Pubkey) -> gacha_rust::Pull {
        let account = self.account(pull);
        gacha_rust::Pull::from_account(*pull, account.owner, &account.data).unwrap()
    }

    pub fn client_asset(&self, key: Pubkey) -> gacha_rust::Asset {
        let account = self.account(&key);
        gacha_rust::Asset::from_account(key, account.owner, &account.data).unwrap()
    }

    pub fn client_settlement(&self, pull: &Pubkey) -> gacha_rust::Settlement {
        let pull = self.client_pull(pull);
        let proof = self.operator_key.prove(&pull.alpha());
        self.client_pool().settle(&pull, &proof).unwrap()
    }

    pub fn client_item(&self, key: Pubkey) -> gacha_rust::Item {
        let account = self.account(&key);
        gacha_rust::Item::from_account(key, account.owner, &account.data).unwrap()
    }

    pub fn client_items(&self, settlement: &gacha_rust::Settlement) -> Vec<gacha_rust::Item> {
        settlement
            .draws()
            .iter()
            .map(|draw| self.client_item(draw.item))
            .collect()
    }

    pub fn create_pool_ix(&self) -> Instruction {
        let mut data = vec![];
        data.push(0u8);
        data.extend(1u64.to_le_bytes());
        data.extend(PRICE.to_le_bytes());
        data.extend(DEADLINE_SLOTS.to_le_bytes());
        data.extend(BOND_PER_DRAW.to_le_bytes());
        data.extend(BOND.to_le_bytes());
        data.push(WEIGHTS.len() as u8);
        let mut weights = [0u32; 8];
        weights[..WEIGHTS.len()].copy_from_slice(&WEIGHTS);
        for w in weights {
            data.extend(w.to_le_bytes());
        }
        Instruction::new_with_bytes(
            PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new(self.authority, true),
                AccountMeta::new(self.pool, false),
                AccountMeta::new_readonly(self.operator, false),
                AccountMeta::new_readonly(self.payment_mint, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(ata(&self.authority, &self.payment_mint), false),
                AccountMeta::new_readonly(TOKEN, false),
                AccountMeta::new_readonly(SYSTEM, false),
            ],
        )
    }

    /// Bootstrap an active pool for flow tests; lifecycle tests create it explicitly.
    pub fn open_pool(&mut self) -> InstructionResult {
        let result = self.run(&self.create_pool_ix());
        if result.program_result.is_ok() {
            self.run(
                &self
                    .client_pool()
                    .set_status(gacha_rust::PoolStatus::Active)
                    .unwrap(),
            )
        } else {
            result
        }
    }

    /// Mint a fresh NFT to the authority and deposit it into `tier`.
    pub fn deposit(&mut self, tier: u8) -> InstructionResult {
        let ix = self.deposit_ix(tier);
        let result = self.run(&ix);
        if result.program_result.is_ok() {
            self.model.items.push(Some((tier, ix.accounts[3].pubkey)));
        }
        result
    }

    pub fn deposit_ix(&mut self, tier: u8) -> Instruction {
        let asset = Pubkey::new_unique();
        self.upsert(asset, core_asset(&self.authority, None));
        let position = self.model.items.len() as u32;
        let data = [1u8, tier];
        Instruction::new_with_bytes(
            PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new(self.authority, true),
                AccountMeta::new(self.pool, false),
                AccountMeta::new(item_pda(&self.pool, tier, position), false),
                AccountMeta::new(asset, false),
                AccountMeta::new_readonly(CORE, false),
                AccountMeta::new_readonly(CORE, false),
                AccountMeta::new_readonly(SYSTEM, false),
            ],
        )
    }

    pub fn buy_ix(&self, count: u8, seed: [u8; 32]) -> Instruction {
        let mut data = vec![10u8, count];
        data.extend(seed);
        data.extend((self.model.items.len() as u32).to_le_bytes());
        Instruction::new_with_bytes(
            PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new(self.buyer, true),
                AccountMeta::new(self.pool, false),
                AccountMeta::new(pull_pda(&self.pool, self.next_index), false),
                AccountMeta::new(ata(&self.buyer, &self.payment_mint), false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new_readonly(TOKEN, false),
                AccountMeta::new_readonly(SYSTEM, false),
            ],
        )
    }

    pub fn buy(&mut self, count: u8, seed: [u8; 32]) -> (Pubkey, InstructionResult) {
        let pull = pull_pda(&self.pool, self.next_index);
        let result = self.run(&self.buy_ix(count, seed));
        if result.program_result.is_ok() {
            self.next_index += 1;
        }
        (pull, result)
    }

    /// The operator's job: prove, walk the model, pass the item accounts.
    /// Returns the instruction and the expected `(tier, asset)` outcomes.
    pub fn settle_ix(&self, pull: &Pubkey, model: &mut Model) -> (Instruction, Vec<(u8, Pubkey)>) {
        let pull_account = self.account(pull);
        let count = pull_account.data[3] as usize;
        let cutoff = u32::from_le_bytes(pull_account.data[4..8].try_into().unwrap()) as usize;
        let seed: [u8; 32] = pull_account.data[88..120].try_into().unwrap();
        let alpha = solana_sha256_hasher::hashv(&[pull.as_ref(), &seed]).to_bytes();
        let proof = self.operator_key.prove(&alpha);
        let beta = proof
            .verify(&PublicKey(self.operator.to_bytes()), &alpha)
            .unwrap();

        let mut metas = vec![
            AccountMeta::new(self.operator, false),
            AccountMeta::new(self.pool, false),
            AccountMeta::new(*pull, false),
        ];
        let mut outcomes = vec![];
        for i in 0..count {
            let hash = solana_sha256_hasher::hashv(&[&beta, &[i as u8]]).to_bytes();
            // A sold-out pool has no item to pass; the program rejects the settle.
            let Some((tier, position, asset)) = model.draw(&hash, cutoff) else {
                break;
            };
            metas.push(AccountMeta::new(
                item_pda(&self.pool, tier, position),
                false,
            ));
            outcomes.push((tier, asset));
        }
        let mut data = vec![20u8];
        data.extend(proof.0);
        (
            Instruction::new_with_bytes(PROGRAM_ID, &data, metas),
            outcomes,
        )
    }

    pub fn settle(&mut self, pull: &Pubkey) -> (InstructionResult, Vec<(u8, Pubkey)>) {
        let mut model = self.model.clone();
        let (ix, outcomes) = self.settle_ix(pull, &mut model);
        let result = self.run(&ix);
        if result.program_result.is_ok() {
            self.model = model;
        }
        (result, outcomes)
    }

    pub fn refund(&mut self, pull: &Pubkey) -> InstructionResult {
        let ix = Instruction::new_with_bytes(
            PROGRAM_ID,
            &[11u8],
            vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new(*pull, false),
                AccountMeta::new(self.buyer, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(ata(&self.buyer, &self.payment_mint), false),
                AccountMeta::new_readonly(TOKEN, false),
            ],
        );
        self.run(&ix)
    }

    pub fn withdraw(&mut self, amount: u64) -> InstructionResult {
        let mut data = vec![2u8];
        data.extend(amount.to_le_bytes());
        let ix = Instruction::new_with_bytes(
            PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new_readonly(self.authority, true),
                AccountMeta::new_readonly(self.pool, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(ata(&self.authority, &self.payment_mint), false),
                AccountMeta::new_readonly(TOKEN, false),
            ],
        );
        self.run(&ix)
    }

    /// Decode a `Pool` field by offset, for assertions.
    pub fn pool_u64(&self, offset: usize) -> u64 {
        u64::from_le_bytes(self.pool_state()[offset..offset + 8].try_into().unwrap())
    }

    pub fn tier_remaining(&self, tier: usize) -> u32 {
        let off = 192 + tier * TIER_LEN + 4;
        u32::from_le_bytes(self.pool_state()[off..off + 4].try_into().unwrap())
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Pool header offsets, mirrored from `state.rs` for assertions.
pub const OFF_PENDING_DRAWS: usize = 8 + 128 + 32;
pub const OFF_NEXT_INDEX: usize = OFF_PENDING_DRAWS + 8;
pub const OFF_NEXT_SETTLE: usize = OFF_NEXT_INDEX + 8;

pub fn custom_error(result: &InstructionResult) -> Option<u32> {
    match &result.program_result {
        mollusk_svm::result::ProgramResult::Failure(
            solana_program_error::ProgramError::Custom(c),
        ) => Some(*c),
        _ => None,
    }
}
