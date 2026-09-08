use ed25519_dalek::{Signer, SigningKey};
use gacha_rust::{Buy, BuybackQuote, CreatePool, Error, Item, Pool, PoolStatus, Pull};
use gacha_tests::*;
use mpl_core::{
    instructions::{CreateCollectionV2Builder, CreateV2Builder},
    types::DataState,
};
use serde_json::{json, Value};
use solana_ecvrf::SecretKey;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn wire(ix: &Instruction) -> Value {
    json!({"programAddress": ix.program_id.to_string(), "data": hex(&ix.data),
        "accounts": ix.accounts.iter().map(|a| json!({"address": a.pubkey.to_string(),
            "role": u8::from(a.is_writable) + 2 * u8::from(a.is_signer)})).collect::<Vec<_>>()})
}
fn groups(instructions: &[Vec<Instruction>]) -> Value {
    let encoded = json!(instructions
        .iter()
        .map(|g| g.iter().map(wire).collect::<Vec<_>>())
        .collect::<Vec<_>>())
    .to_string();
    json!(hex(
        &solana_sha256_hasher::hash(encoded.as_bytes()).to_bytes()
    ))
}
fn account(f: &Fixture, key: Pubkey) -> Value {
    json!({"address": key.to_string(), "owner": f.account(&key).owner.to_string(), "data": hex(&f.account(&key).data)})
}

/// Executable source of the TypeScript parity vector. To regenerate its JSON:
/// GACHA_PRINT_VECTOR=1 cargo test -p gacha-tests --test client -- --nocapture
#[test]
fn client_flow_and_trust_boundaries() {
    let mut f = Fixture::new();
    let authority = f.authority;
    let operator = f.operator;
    let create = CreatePool {
        id: 1,
        price: PRICE,
        deadline_slots: DEADLINE_SLOTS,
        bond_per_draw: BOND_PER_DRAW,
        bond: BOND,
        weights: WEIGHTS.to_vec(),
    }
    .instructions(f.authority, f.operator, f.payment_mint)
    .unwrap();
    assert_eq!(create[1], f.create_pool_ix());
    f.upsert(f.vault, wallet(0)); // SDK creates missing custody accounts itself.
    assert!(f.run_transaction(&create, &authority).raw_result.is_ok());
    // Different collection accounts exercise packet splitting as well as Core routing.
    let collections: Vec<_> = (0..10)
        .map(|_| {
            let collection = Pubkey::new_unique();
            let create = CreateCollectionV2Builder::new()
                .collection(collection)
                .payer(authority)
                .name("Prizes".into())
                .uri(String::new())
                .instruction();
            assert!(f.run(&create).program_result.is_ok());
            collection
        })
        .collect();
    for i in 0..69 {
        let asset = Pubkey::new_unique();
        assert!(f
            .run(
                &CreateV2Builder::new()
                    .asset(asset)
                    .collection(Some(collections[i as usize % collections.len()]))
                    .payer(authority)
                    .data_state(DataState::AccountState)
                    .name("Prize".into())
                    .uri(String::new())
                    .instruction()
            )
            .program_result
            .is_ok());
        let deposit = f
            .client_pool()
            .deposit(i % 3, &f.client_asset(asset))
            .unwrap();
        assert!(f.run(&deposit).program_result.is_ok());
        f.model.items.push(Some((i % 3, asset)));
    }
    let before_deposit = account(&f, f.pool);
    let deposit_ix = f.deposit_ix(1);
    let asset = deposit_ix.accounts[3].pubkey;
    let deposit_asset = account(&f, asset);
    let deposit = f.client_pool().deposit(1, &f.client_asset(asset)).unwrap();
    assert_eq!(deposit, deposit_ix);
    assert!(f
        .run_transaction(std::slice::from_ref(&deposit), &authority)
        .raw_result
        .is_ok());
    f.model.items.push(Some((1, asset)));

    assert_eq!(f.client_pool().status(), PoolStatus::Paused);
    let activate = f.client_pool().set_status(PoolStatus::Active).unwrap();
    assert!(f.run(&activate).program_result.is_ok());
    let before_buy = account(&f, f.pool);
    let buy = f
        .client_pool()
        .buy(Buy {
            buyer: f.buyer,
            count: 10,
            client_seed: [7; 32],
        })
        .unwrap();
    assert_eq!(buy.instruction, f.buy_ix(10, [7; 32]));
    assert_eq!(buy.pull, pull_pda(&f.pool, 0));
    assert!(f.run(&buy.instruction).program_result.is_ok());
    assert!(f.deposit(2).program_result.is_ok()); // Not eligible for this purchase.
    let pending_pool = account(&f, f.pool);
    let pending_pull = account(&f, buy.pull);
    let pool = f.client_pool();
    let pull = f.client_pull(&buy.pull);
    let proof = f.operator_key.prove(&pull.alpha());
    let plan = pool.settle(&pull, &proof).unwrap();
    assert_eq!(
        plan.instruction(),
        f.settle_ix(&buy.pull, &mut f.model.clone()).0
    );
    let items = f.client_items(&plan);
    let item_accounts: Vec<_> = items.iter().map(|i| account(&f, i.address())).collect();
    let assets: Vec<_> = items.iter().map(|i| f.client_asset(i.asset())).collect();
    let asset_accounts: Vec<_> = assets.iter().map(|a| account(&f, a.address)).collect();
    let instructions = plan.instructions(&items, &assets, f.operator).unwrap();
    assert!(instructions.len() > 1);
    let refund = pool.refund(&pull, operator).unwrap();
    let withdraw = pool.withdraw(ata(&f.authority, &f.payment_mint), u64::MAX);

    assert_eq!(
        pool.settle(&pull, &SecretKey([8; 32]).prove(&pull.alpha()))
            .unwrap_err(),
        Error::InvalidProof
    );
    assert_eq!(
        plan.instructions(&[], &[], f.operator).unwrap_err(),
        Error::InvalidItem
    );
    assert_eq!(
        pull.deliver(&[], f.operator).unwrap_err(),
        Error::NotSettled
    );
    assert_eq!(
        Pool::from_account(f.pool, TOKEN, &f.pool_state()).unwrap_err(),
        Error::InvalidAccount
    );
    let header = Pool::from_account(f.pool, PROGRAM_ID, &f.pool_state()[..256]).unwrap();
    assert_eq!(
        header.settle(&pull, &proof).unwrap_err(),
        Error::MissingInventory
    );
    assert_eq!(
        Pull::from_account(buy.pull, PROGRAM_ID, &f.account(&buy.pull).data[..449]).unwrap_err(),
        Error::InvalidAccount
    );
    assert_eq!(
        Item::from_account(items[0].address(), PROGRAM_ID, &[1; 72]).unwrap_err(),
        Error::InvalidAccount
    );

    // Settle and deliver one award, then recover only the remaining awards.
    assert!(f
        .run_transaction(&instructions[0][..2], &operator)
        .raw_result
        .is_ok());
    let partial_pull = account(&f, buy.pull);
    let pending_assets: Vec<_> = f
        .client_pull(&buy.pull)
        .outcomes()
        .unwrap()
        .iter()
        .filter(|o| o.tier.is_some())
        .map(|o| f.client_asset(o.asset))
        .collect();
    let recovery_assets: Vec<_> = pending_assets
        .iter()
        .map(|a| account(&f, a.address))
        .collect();
    let recovery = f
        .client_pull(&buy.pull)
        .deliver(&pending_assets, f.operator)
        .unwrap();
    assert_eq!(
        recovery.iter().flatten().collect::<Vec<_>>(),
        instructions.iter().flatten().skip(2).collect::<Vec<_>>()
    );
    for group in &recovery {
        assert!(f.run_transaction(group, &operator).raw_result.is_ok());
    }
    assert_eq!(f.account(&buy.pull).lamports, 0);
    for item in &items {
        assert_eq!(f.client_asset(item.asset()).owner, f.buyer);
    }

    let buyback_pool = account(&f, f.pool);
    let quote = BuybackQuote {
        pool: f.pool,
        asset: items[0].asset(),
        price: PRICE,
        expires_at: 300,
        tier: 1,
    };
    let signature = SigningKey::from_bytes(&AUTHORITY_SEED)
        .sign(&quote.message())
        .to_bytes();
    let buyback_asset = account(&f, quote.asset);
    let buyback = f
        .client_pool()
        .buyback(&quote, &signature, &f.client_asset(quote.asset), operator)
        .unwrap();
    assert!(f.run_transaction(&buyback, &operator).raw_result.is_ok());

    let retire = f.client_pool().set_status(PoolStatus::Retired).unwrap();
    assert!(f.run(&retire).program_result.is_ok());
    let retired_pool = account(&f, f.pool);
    let item_key = Item::address_for(f.pool, quote.tier, f.client_pool().inventory_version() - 1);
    let reclaim_item = account(&f, item_key);
    let reclaim_asset = account(&f, quote.asset);
    let reclaim = f
        .client_pool()
        .reclaim(&f.client_item(item_key), &f.client_asset(quote.asset))
        .unwrap();
    assert!(f.run(&reclaim).program_result.is_ok());
    assert_eq!(f.client_asset(quote.asset).owner, authority);

    let vector = json!({"authority": f.authority.to_string(),
            "operator": f.operator.to_string(), "buyer": f.buyer.to_string(), "paymentMint": f.payment_mint.to_string(),
            "activate": wire(&activate), "retire": wire(&retire), "retiredPool": retired_pool, "reclaimItem": reclaim_item, "reclaimAsset": reclaim_asset, "reclaim": wire(&reclaim),
            "beforeDeposit": before_deposit, "depositAsset": deposit_asset, "beforeBuy": before_buy,
            "pool": pending_pool, "pull": pending_pull, "partialPull": partial_pull,
            "proof": hex(&proof.0), "items": item_accounts, "assets": asset_accounts, "recoveryAssets": recovery_assets, "create": create.iter().map(wire).collect::<Vec<_>>(),
            "deposit": wire(&deposit), "buy": wire(&buy.instruction),
            "settle": wire(&plan.instruction()), "refund": refund.iter().map(wire).collect::<Vec<_>>(), "withdraw": wire(&withdraw),
            "instructions": groups(&instructions), "recovery": groups(&recovery),
            "buybackPool": buyback_pool, "buybackAsset": buyback_asset,
            "buybackQuote": {"pool": quote.pool.to_string(), "asset": quote.asset.to_string(), "price": quote.price.to_string(), "expiresAt": quote.expires_at.to_string(), "tier": quote.tier},
            "buybackMessage": hex(&quote.message()), "buybackSignature": hex(&signature),
            "buyback": buyback.iter().map(wire).collect::<Vec<_>>()});
    if std::env::var_os("GACHA_PRINT_VECTOR").is_some() {
        println!("CLIENT_VECTOR={vector}");
    }
    assert_eq!(
        vector,
        serde_json::from_str::<Value>(include_str!("../fixtures/client.json")).unwrap(),
        "client vector is stale; regenerate with GACHA_PRINT_VECTOR=1"
    );
}
