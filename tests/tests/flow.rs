use gacha_program::test_utils::GachaError;
use gacha_tests::*;
use mollusk_svm::result::types::TransactionProgramResult;
use solana_instruction::{AccountMeta, Instruction};
use solana_program_error::ProgramError;

fn seeded(n: u8) -> [u8; 32] {
    [n; 32]
}

#[test]
fn deposit_rejects_payment_vault_as_prize_custody() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    let mut ix = f.deposit_ix(0);
    let source = ata(&f.authority, &f.payment_mint);
    f.upsert(source, token_account(&f.payment_mint, &f.authority, 1));
    ix.accounts[3].pubkey = f.payment_mint;
    ix.accounts[4].pubkey = source;
    ix.accounts[5].pubkey = f.vault;
    let before = [f.pool, f.vault, source, f.authority].map(|key| (key, f.account(&key).clone()));

    let result = f.run(&ix);
    assert_eq!(
        custom_error(&result),
        Some(GachaError::InvalidTokenAddress as u32)
    );
    for (key, account) in before {
        assert_eq!(result.get_account(&key), Some(&account));
    }
}

#[test]
fn wrong_proof_and_wrong_items_rejected() {
    let mut f = Fixture::new();
    f.create_pool();
    for tier in 0..3 {
        f.deposit(tier);
        f.deposit(tier);
    }
    let (pull, _) = f.buy(1, seeded(3));
    let (mut ix, _) = f.settle_ix(&pull, &mut f.model.clone());
    // Flip one byte of the proof.
    let good = ix.clone();
    ix.data[5] ^= 1;
    assert_eq!(
        custom_error(&f.run(&ix)),
        Some(GachaError::InvalidProof as u32)
    );
    // Pass a different (valid) item account for the draw.
    let mut ix = good.clone();
    let swapped = ix.accounts[3].pubkey;
    let other = f
        .model
        .items
        .iter()
        .enumerate()
        .filter_map(|(position, item)| {
            item.map(|(tier, _)| item_pda(&f.pool, tier, position as u32))
        })
        .find(|k| *k != swapped)
        .unwrap();
    ix.accounts[3].pubkey = other;
    assert_eq!(
        custom_error(&f.run(&ix)),
        Some(GachaError::InvalidItem as u32)
    );
    // The good one still settles.
    assert!(f.run(&good).program_result.is_ok());
}

#[test]
fn refund_recreates_a_closed_payment_ata_and_advances_queue() {
    let mut f = Fixture::new();
    f.create_pool();
    f.deposit(0);
    f.deposit(0);
    let buyer_ata = ata(&f.buyer, &f.payment_mint);
    f.upsert(
        buyer_ata,
        token_account(&f.payment_mint, &f.buyer, 2 * PRICE),
    );
    let (pull, result) = f.buy(2, seeded(4));
    assert!(result.program_result.is_ok());
    let close = Instruction::new_with_bytes(
        TOKEN,
        &[9],
        vec![
            AccountMeta::new(buyer_ata, false),
            AccountMeta::new(f.buyer, false),
            AccountMeta::new_readonly(f.buyer, true),
        ],
    );
    assert!(f.run(&close).program_result.is_ok());
    f.mollusk.warp_to_slot(DEADLINE_SLOTS + 1);
    let refund = f.client_pool().refund(&f.client_pull(&pull)).unwrap();
    assert_eq!(
        custom_error(&f.run(&refund)),
        Some(GachaError::InvalidTokenAddress as u32)
    );
    // Native ATA CreateIdempotent + Refund; a relayer pays, the buyer need not sign.
    let payer = f.operator;
    let create_ata = Instruction::new_with_bytes(
        ATA_PROGRAM,
        &[1],
        vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(buyer_ata, false),
            AccountMeta::new_readonly(f.buyer, false),
            AccountMeta::new_readonly(f.payment_mint, false),
            AccountMeta::new_readonly(SYSTEM, false),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    );
    assert!(f
        .run_transaction(&[create_ata, refund], &payer)
        .raw_result
        .is_ok());
    assert_eq!(
        token_amount(f.account(&buyer_ata)),
        2 * (PRICE + BOND_PER_DRAW)
    );
    assert_eq!(token_amount(f.account(&f.vault)), BOND - 2 * BOND_PER_DRAW);
    assert_eq!(f.pool_u64(OFF_PENDING_DRAWS), 0);
    assert_eq!(f.pool_u64(OFF_NEXT_SETTLE), 1);
    assert_eq!(f.account(&pull).lamports, 0);
    // The next pull settles normally after the gap.
    let (pull, _) = f.buy(1, seeded(5));
    assert!(f.settle(&pull).0.program_result.is_ok());
}

#[test]
fn buy_rejects_invalid_counts() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    assert!(f.deposit(0).program_result.is_ok());
    for count in [0, 11] {
        assert_eq!(
            custom_error(&f.buy(count, seeded(0)).1),
            Some(GachaError::InvalidCount as u32)
        );
    }
}

#[test]
fn pending_purchases_cannot_oversubscribe_stock() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    for _ in 0..3 {
        assert!(f.deposit(0).program_result.is_ok());
    }
    let (first, result) = f.buy(2, seeded(1));
    assert!(result.program_result.is_ok());
    assert_eq!(
        custom_error(&f.buy(2, seeded(2)).1),
        Some(GachaError::SoldOut as u32)
    );
    let (second, result) = f.buy(1, seeded(3));
    assert!(result.program_result.is_ok());
    assert_eq!(f.pool_u64(OFF_PENDING_DRAWS), 3);
    assert_eq!(
        custom_error(&f.buy(1, seeded(4)).1),
        Some(GachaError::SoldOut as u32)
    );
    assert!(f.settle(&first).0.program_result.is_ok());
    assert!(f.settle(&second).0.program_result.is_ok());
    assert_eq!(f.pool_u64(OFF_PENDING_DRAWS), 0);
    assert_eq!(
        custom_error(&f.buy(1, seeded(5)).1),
        Some(GachaError::SoldOut as u32)
    );
}

#[test]
fn collateral_covers_every_pending_refund_and_can_be_funded_by_token_transfer() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    for _ in 0..3 {
        assert!(f.deposit(0).program_result.is_ok());
    }
    assert!(f.withdraw(BOND - 2 * BOND_PER_DRAW).program_result.is_ok());
    let (first, result) = f.buy(1, seeded(1));
    assert!(result.program_result.is_ok());
    let (second, result) = f.buy(1, seeded(2));
    assert!(result.program_result.is_ok());
    assert_eq!(
        custom_error(&f.buy(1, seeded(3)).1),
        Some(GachaError::InsufficientBalance as u32)
    );
    assert_eq!(
        custom_error(&f.withdraw(1)),
        Some(GachaError::InsufficientBalance as u32)
    );

    f.mollusk.warp_to_slot(DEADLINE_SLOTS + 1);
    assert_eq!(
        custom_error(&f.refund(&second)),
        Some(GachaError::NotNextInQueue as u32)
    );
    assert!(f.refund(&first).program_result.is_ok());
    assert!(f.refund(&second).program_result.is_ok());
    assert_eq!(token_amount(f.account(&f.vault)), 0);
    assert_eq!(f.pool_u64(OFF_PENDING_DRAWS), 0);

    // Ordinary SPL Transfer into the vault replenishes the timeout collateral.
    let mut data = vec![3];
    data.extend(BOND_PER_DRAW.to_le_bytes());
    let fund = solana_instruction::Instruction::new_with_bytes(
        TOKEN,
        &data,
        vec![
            solana_instruction::AccountMeta::new(ata(&f.authority, &f.payment_mint), false),
            solana_instruction::AccountMeta::new(f.vault, false),
            solana_instruction::AccountMeta::new_readonly(f.authority, true),
        ],
    );
    assert!(f.run(&fund).program_result.is_ok());
    let (pull, result) = f.buy(1, seeded(4));
    assert!(result.program_result.is_ok());
    assert!(f.settle(&pull).0.program_result.is_ok());
    // Settlement releases revenue and unused collateral for withdrawal.
    assert!(f.withdraw(PRICE + BOND_PER_DRAW).program_result.is_ok());
    assert_eq!(token_amount(f.account(&f.vault)), 0);
    assert_eq!(
        custom_error(&f.withdraw(1)),
        Some(GachaError::InsufficientBalance as u32)
    );
}

#[test]
fn prefunded_pool_item_and_pull_accounts_initialize_normally() {
    let mut f = Fixture::new();
    f.upsert(f.pool, wallet(1));
    assert!(f.create_pool().program_result.is_ok());
    f.upsert(item_pda(&f.pool, 0, 0), wallet(1));
    assert!(f.deposit(0).program_result.is_ok());
    let pull = pull_pda(&f.pool, 0);
    // Already fully funded; initialization must not overcharge its payer.
    f.upsert(pull, wallet(10_000_000));
    let before = f.account(&f.buyer).lamports;
    assert!(f.buy(1, seeded(1)).1.program_result.is_ok());
    assert_eq!(f.account(&f.buyer).lamports, before);
    assert!(f.settle(&pull).0.program_result.is_ok());
    assert!(
        f.create_pool().program_result.is_err(),
        "cannot reinitialize an existing pool"
    );
}

#[test]
fn deadline_has_one_unambiguous_settlement_refund_boundary() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    assert!(f.deposit(0).program_result.is_ok());
    let (pull, result) = f.buy(1, seeded(1));
    assert!(result.program_result.is_ok());
    f.mollusk.warp_to_slot(DEADLINE_SLOTS);
    assert_eq!(
        custom_error(&f.refund(&pull)),
        Some(GachaError::DeadlineNotReached as u32)
    );
    assert!(
        f.settle(&pull).0.program_result.is_ok(),
        "settlement allowed at deadline"
    );

    assert!(f.deposit(0).program_result.is_ok());
    let (pull, result) = f.buy(1, seeded(2));
    assert!(result.program_result.is_ok());
    f.mollusk.warp_to_slot(2 * DEADLINE_SLOTS + 1);
    assert_eq!(
        custom_error(&f.settle(&pull).0),
        Some(GachaError::DeadlinePassed as u32)
    );
    assert!(f.refund(&pull).program_result.is_ok());
}

#[test]
fn invalid_prices_penalties_deadlines_and_vrf_keys_are_rejected() {
    for (offset, value) in [
        (9, u64::MAX),
        (9, u64::MAX / 10),
        (9, 0),
        (17, 0),
        (25, 0),
        (25, u64::MAX),
    ] {
        let mut f = Fixture::new();
        let mut ix = f.create_pool_ix();
        ix.data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        assert_eq!(
            custom_error(&f.run(&ix)),
            Some(GachaError::InvalidPoolParams as u32)
        );
    }
    let mut f = Fixture::new();
    let mut ix = f.create_pool_ix();
    ix.accounts[2].pubkey = SYSTEM;
    assert_eq!(
        custom_error(&f.run(&ix)),
        Some(GachaError::InvalidOperator as u32)
    );
}

#[test]
fn overflowing_deadline_or_purchase_index_is_rejected() {
    let mut f = Fixture::new();
    let mut ix = f.create_pool_ix();
    ix.data[17..25].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(f.run(&ix).program_result.is_ok());
    assert!(f.deposit(0).program_result.is_ok());
    f.mollusk.warp_to_slot(1);
    assert!(f.buy(1, seeded(1)).1.program_result.is_err());

    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    assert!(f.deposit(0).program_result.is_ok());
    let mut pool = f.account(&f.pool).clone();
    pool.data[OFF_NEXT_INDEX..OFF_NEXT_INDEX + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    f.upsert(f.pool, pool);
    f.next_index = u64::MAX;
    assert!(f.buy(1, seeded(1)).1.program_result.is_err());
}

#[test]
fn composed_settlement_creates_missing_atas_and_delivers_one_to_ten_draws() {
    for count in 1..=10 {
        let mut f = Fixture::new();
        assert!(f.create_pool().program_result.is_ok());
        for i in 0..30 {
            assert!(f.deposit((i % 3) as u8).program_result.is_ok());
        }
        let (pull, result) = f.buy(count, seeded(count));
        assert!(result.program_result.is_ok());
        let (settle, outcomes) = f.settle_ix(&pull, &mut f.model.clone());
        assert!(
            !settle.accounts[0].is_signer,
            "operator only publishes the proof"
        );
        let mints: Vec<_> = outcomes.iter().map(|(_, mint)| *mint).collect();
        // A distinct payer relays the proof and funds ATA creation.
        let payer = solana_pubkey::Pubkey::new_unique();
        f.upsert(payer, wallet(100_000_000));
        let plan = f.client_settlement(&pull);
        assert_eq!(plan.instruction(), settle);
        let transactions = plan.instructions(&f.client_items(&plan), payer).unwrap();
        if count == 1 {
            assert_eq!(transactions.len(), 1);
        }
        if count == 10 {
            assert!(transactions.len() > 1);
        }
        for instructions in transactions {
            let message = solana_message::Message::new(&instructions, Some(&payer));
            assert_eq!(
                message.header.num_required_signatures, 1,
                "only the relayer signs"
            );
            assert!(65 + message.serialize().len() <= 1232);
            let result = f.run_transaction(&instructions, &payer);
            assert!(result.raw_result.is_ok(), "{:?}", result.raw_result);
        }
        assert_eq!(f.pool_u64(OFF_PENDING_DRAWS), 0);
        assert_eq!(f.pool_u64(OFF_NEXT_SETTLE), 1);
        assert_eq!(
            (0..3).map(|tier| f.tier_remaining(tier)).sum::<u32>(),
            30 - count as u32
        );
        assert_eq!(f.account(&pull).lamports, 0);
        for mint in mints {
            assert_eq!(token_amount(f.account(&ata(&f.buyer, &mint))), 1);
        }
    }
}

#[test]
fn composed_single_draw_rolls_back_settlement_when_delivery_fails() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    assert!(f.deposit(0).program_result.is_ok());
    let (pull, result) = f.buy(1, seeded(1));
    assert!(result.program_result.is_ok());
    let (settle, outcomes) = f.settle_ix(&pull, &mut f.model.clone());
    let mint = outcomes[0].1;
    let plan = f.client_settlement(&pull);
    assert_eq!(plan.instruction(), settle);
    let mut transactions = plan
        .instructions(&f.client_items(&plan), f.operator)
        .unwrap();
    let instructions = &mut transactions[0];
    // An unrelated owner must not receive the prize, even in a composed tx.
    let stranger = solana_pubkey::Pubkey::new_unique();
    let destination = ata(&stranger, &mint);
    f.upsert(destination, token_account(&mint, &stranger, 0));
    instructions.last_mut().unwrap().accounts[4].pubkey = destination;
    let before = [f.pool, pull, settle.accounts[3].pubkey, ata(&f.pool, &mint)]
        .map(|key| (key, f.account(&key).clone()));
    let payer = f.operator;
    let result = f.run_transaction(instructions, &payer);
    assert_eq!(
        result.program_result,
        TransactionProgramResult::Failure(
            instructions.len() - 1,
            ProgramError::Custom(GachaError::InvalidTokenAddress as u32)
        )
    );
    for (key, account) in before {
        assert_eq!(result.get_account(&key), Some(&account));
    }
}

#[test]
fn failed_payment_cannot_reserve_stock_or_create_a_pull() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    assert!(f.deposit(0).program_result.is_ok());
    f.upsert(
        ata(&f.buyer, &f.payment_mint),
        token_account(&f.payment_mint, &f.buyer, 0),
    );
    let ix = f.buy_ix(1, seeded(1));
    f.ensure(&ix);
    let before =
        [f.pool, ix.accounts[2].pubkey, f.vault, f.buyer].map(|key| (key, f.account(&key).clone()));
    let payer = f.operator;
    let result = f.run_transaction(&[ix], &payer);
    assert_eq!(
        result.program_result,
        TransactionProgramResult::Failure(
            0,
            ProgramError::Custom(1) // SPL Token: insufficient funds.
        )
    );
    for (key, account) in before {
        assert_eq!(result.get_account(&key), Some(&account));
    }
}

#[test]
fn restocking_cannot_change_a_pending_draw_but_new_purchases_can_win_new_stock() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    for _ in 0..4 {
        assert!(f.deposit(2).program_result.is_ok());
    }
    let (old, result) = f.buy(3, seeded(11));
    assert!(result.program_result.is_ok());
    let (prepared, expected) = f.settle_ix(&old, &mut f.model.clone());
    // Cross multiple index blocks and introduce tiers absent at purchase.
    for i in 0..130 {
        assert!(f.deposit((i % 3) as u8).program_result.is_ok());
    }
    let (new, result) = f.buy(10, seeded(12));
    assert!(result.program_result.is_ok());
    assert_eq!(f.settle_ix(&old, &mut f.model.clone()).0, prepared);
    let (result, actual) = f.settle(&old);
    assert!(result.program_result.is_ok(), "{:?}", result.program_result);
    assert_eq!(actual, expected);
    assert!(actual.iter().all(|(tier, _)| *tier == 2));
    let (result, _) = f.settle(&new);
    assert!(result.program_result.is_ok(), "{:?}", result.program_result);
    assert!(
        f.model.items[4..]
            .iter()
            .filter(|item| item.is_none())
            .count()
            >= 9
    );
}

#[test]
fn overlapping_versions_deplete_only_their_eligible_stock_in_fifo_order() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    for _ in 0..3 {
        assert!(f.deposit(0).program_result.is_ok());
    }
    let (a, result) = f.buy(2, seeded(21));
    assert!(result.program_result.is_ok());
    assert!(f.deposit(1).program_result.is_ok());
    let (b, result) = f.buy(2, seeded(22));
    assert!(result.program_result.is_ok());
    assert!(f.deposit(2).program_result.is_ok());
    let (c, result) = f.buy(1, seeded(23));
    assert!(result.program_result.is_ok());
    // Replenishing a tier must not resurrect it for older purchases.
    assert!(f.deposit(0).program_result.is_ok());
    assert!(f.deposit(1).program_result.is_ok());
    assert_eq!(
        custom_error(&f.settle(&b).0),
        Some(GachaError::NotNextInQueue as u32)
    );
    assert!(f.settle(&a).0.program_result.is_ok());
    let (result, b_outcomes) = f.settle(&b);
    assert!(result.program_result.is_ok(), "{:?}", result.program_result);
    let mut tiers: Vec<_> = b_outcomes.iter().map(|(t, _)| *t).collect();
    tiers.sort();
    assert_eq!(tiers, [0, 1]);
    let (result, outcomes) = f.settle(&c);
    assert!(result.program_result.is_ok(), "{:?}", result.program_result);
    assert_eq!(outcomes[0].0, 2);
    assert_eq!(f.tier_remaining(0), 1);
    assert_eq!(f.tier_remaining(1), 1);
    assert_eq!(f.pool_u64(OFF_PENDING_DRAWS), 0);
}

#[test]
fn signed_inventory_version_rejects_restocking_before_payment_and_client_rebuilds() {
    use gacha_program::client::Buy;
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    assert!(f.deposit(0).program_result.is_ok());
    let prepared = f
        .client_pool()
        .buy(Buy {
            buyer: f.buyer,
            count: 1,
            client_seed: seeded(31),
        })
        .unwrap();
    assert!(f.deposit(2).program_result.is_ok());
    assert_eq!(
        custom_error(&f.run(&prepared.instruction)),
        Some(GachaError::InventoryChanged as u32)
    );
    let fresh = f
        .client_pool()
        .buy(Buy {
            buyer: f.buyer,
            count: 1,
            client_seed: seeded(32),
        })
        .unwrap();
    assert!(f.run(&fresh.instruction).program_result.is_ok());
    assert!(f.settle(&pull_pda(&f.pool, 0)).0.program_result.is_ok());
}

#[test]
fn failed_deposit_rolls_back_inventory_growth_version_and_rent() {
    let mut f = Fixture::new();
    assert!(f.create_pool().program_result.is_ok());
    for i in 0..64 {
        assert!(f.deposit((i % 3) as u8).program_result.is_ok());
    }
    let ix = f.deposit_ix(0);
    let mint = ix.accounts[3].pubkey;
    f.upsert(ix.accounts[4].pubkey, token_account(&mint, &f.authority, 0));
    let before = f.account(&f.pool).clone();
    let payer = f.authority;
    let result = f.run_transaction(&[ix], &payer);
    assert_eq!(
        result.program_result,
        TransactionProgramResult::Failure(
            0,
            ProgramError::Custom(1) // SPL Token: insufficient funds.
        )
    );
    assert_eq!(result.get_account(&f.pool), Some(&before));
    assert!(f.deposit(1).program_result.is_ok());
    assert_eq!(f.account(&f.pool).data.len(), before.data.len() + 96);
}
