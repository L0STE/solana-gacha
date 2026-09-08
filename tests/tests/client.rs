use gacha_program::client::{Buy, CreatePool, Error, Item, Pool, Pull};
use gacha_tests::*;
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
    for i in 0..69 {
        assert!(f.deposit(i % 3).program_result.is_ok());
    }
    let before_deposit = account(&f, f.pool);
    let deposit_ix = f.deposit_ix(1);
    let mint = deposit_ix.accounts[3].pubkey;
    let deposit = f.client_pool().deposit(1, mint).unwrap();
    assert_eq!(deposit[1], deposit_ix);
    f.upsert(ata(&f.pool, &mint), wallet(0));
    assert!(f.run_transaction(&deposit, &authority).raw_result.is_ok());
    f.model.items.push(Some((1, mint)));

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
    let instructions = plan.instructions(&items, f.operator).unwrap();
    let refund = pool.refund(&pull).unwrap();
    let withdraw = pool.withdraw(ata(&f.authority, &f.payment_mint), u64::MAX);

    assert_eq!(
        pool.settle(&pull, &SecretKey([8; 32]).prove(&pull.alpha()))
            .unwrap_err(),
        Error::InvalidProof
    );
    assert_eq!(
        plan.instructions(&[], f.operator).unwrap_err(),
        Error::InvalidItem
    );
    assert_eq!(pull.deliver(f.operator).unwrap_err(), Error::NotSettled);
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

    // Stop after the first packet and recover only the recorded undelivered awards.
    assert!(f
        .run_transaction(&instructions[0], &operator)
        .raw_result
        .is_ok());
    let partial_pull = account(&f, buy.pull);
    let recovery = f.client_pull(&buy.pull).deliver(f.operator).unwrap();
    assert_eq!(recovery, instructions[1..]);
    for group in &recovery {
        assert!(f.run_transaction(group, &operator).raw_result.is_ok());
    }
    assert_eq!(f.account(&buy.pull).lamports, 0);
    for item in items {
        assert_eq!(token_amount(f.account(&ata(&f.buyer, &item.mint()))), 1);
    }

    let vector = json!({"authority": f.authority.to_string(),
            "operator": f.operator.to_string(), "buyer": f.buyer.to_string(), "paymentMint": f.payment_mint.to_string(),
            "beforeDeposit": before_deposit, "depositMint": mint.to_string(), "beforeBuy": before_buy,
            "pool": pending_pool, "pull": pending_pull, "partialPull": partial_pull,
            "proof": hex(&proof.0), "items": item_accounts, "create": create.iter().map(wire).collect::<Vec<_>>(),
            "deposit": deposit.iter().map(wire).collect::<Vec<_>>(), "buy": wire(&buy.instruction),
            "settle": wire(&plan.instruction()), "refund": wire(&refund), "withdraw": wire(&withdraw),
            "instructions": groups(&instructions), "recovery": groups(&recovery)});
    if std::env::var_os("GACHA_PRINT_VECTOR").is_some() {
        println!("CLIENT_VECTOR={vector}");
    }
    assert_eq!(
        vector,
        serde_json::from_str::<Value>(include_str!("../../program/src/client/test-vector.json"))
            .unwrap(),
        "client vector is stale; regenerate with GACHA_PRINT_VECTOR=1"
    );
}
