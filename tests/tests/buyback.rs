use ed25519_dalek::{Signer, SigningKey};
use gacha_rust::{BuybackQuote, GachaError, PoolStatus};
use gacha_tests::*;
use solana_instruction::Instruction;

fn setup() -> (Fixture, BuybackQuote, [u8; 64]) {
    let mut f = Fixture::new();
    assert!(f.open_pool().program_result.is_ok());
    assert!(f.deposit(0).program_result.is_ok());
    let asset = f.deposit_ix(1).accounts[3].pubkey;
    f.upsert(asset, core_asset(&f.buyer, None));
    f.mollusk.sysvars.clock.unix_timestamp = 1_700_000_000;
    let quote = BuybackQuote {
        pool: f.pool,
        asset,
        price: PRICE,
        expires_at: 1_700_000_300,
        tier: 1,
    };
    let signature = SigningKey::from_bytes(&AUTHORITY_SEED)
        .sign(&quote.message())
        .to_bytes();
    (f, quote, signature)
}

fn instructions(f: &Fixture, quote: &BuybackQuote, signature: &[u8; 64]) -> Vec<Instruction> {
    f.client_pool()
        .buyback(quote, signature, &f.client_asset(quote.asset), f.operator)
        .unwrap()
}

#[test]
fn buyback_restocks_after_the_signed_prefix_and_the_quote_can_be_reused() {
    let (mut f, quote, signature) = setup();
    let payer = f.operator;
    let (old_pull, result) = f.buy(1, [1; 32]);
    assert!(result.program_result.is_ok());
    let balance = token_amount(f.account(&ata(&f.buyer, &f.payment_mint)));
    let ix = instructions(&f, &quote, &signature);
    assert!(f.run_transaction(&ix, &payer).raw_result.is_ok());
    assert_eq!(
        token_amount(f.account(&ata(&f.buyer, &f.payment_mint))),
        balance + quote.price
    );
    assert_eq!(f.client_asset(quote.asset).owner, f.pool);
    f.model.items.push(Some((quote.tier, quote.asset)));
    assert_eq!(f.client_pool().inventory_version(), 2);
    assert_eq!(f.client_pool().tiers()[1].remaining, 1);
    let (result, _) = f.settle(&old_pull);
    assert!(result.program_result.is_ok());
    assert_ne!(
        f.client_pull(&old_pull).outcomes().unwrap()[0].asset,
        quote.asset
    );

    // A repeat needs the asset back: draw/deliver the restocked prize first.
    let (pull, result) = f.buy(1, [2; 32]);
    assert!(result.program_result.is_ok());
    assert!(f.settle(&pull).0.program_result.is_ok());
    for group in f
        .client_pull(&pull)
        .deliver(&[f.client_asset(quote.asset)], payer)
        .unwrap()
    {
        assert!(f.run_transaction(&group, &payer).raw_result.is_ok());
    }
    let ix = instructions(&f, &quote, &signature);
    assert!(f.run_transaction(&ix, &payer).raw_result.is_ok());
    assert_eq!(f.client_pool().inventory_version(), 3);
    assert_eq!(f.client_asset(quote.asset).owner, f.pool);
}

#[test]
fn buyback_binds_terms_and_seller_authorization_and_expires_at_the_deadline() {
    let (mut f, quote, signature) = setup();
    let good = instructions(&f, &quote, &signature).pop().unwrap();
    let pause = f.client_pool().set_status(PoolStatus::Paused).unwrap();
    assert!(f.run(&pause).program_result.is_ok());
    assert_eq!(
        custom_error(&f.run(&good)),
        Some(GachaError::InvalidPoolStatus as u32)
    );
    let activate = f.client_pool().set_status(PoolStatus::Active).unwrap();
    assert!(f.run(&activate).program_result.is_ok());
    // Each signed field, including the implicit pool/asset accounts, is bound.
    for offset in [1, 2, 10, 18] {
        let mut ix = good.clone();
        ix.data[offset] ^= 1;
        assert_eq!(
            custom_error(&f.run(&ix)),
            Some(GachaError::InvalidQuote as u32)
        );
    }
    let mut ix = good.clone();
    // A different asset account is bound by the signature, checked before the Core transfer.
    ix.accounts[4].pubkey = f.payment_mint;
    assert_eq!(
        custom_error(&f.run(&ix)),
        Some(GachaError::InvalidQuote as u32)
    );
    let other = BuybackQuote {
        pool: f.buyer,
        ..quote
    };
    let mut ix = good.clone();
    ix.data[18..82].copy_from_slice(
        &SigningKey::from_bytes(&AUTHORITY_SEED)
            .sign(&other.message())
            .to_bytes(),
    );
    assert_eq!(
        custom_error(&f.run(&ix)),
        Some(GachaError::InvalidQuote as u32)
    );
    let mut ix = good.clone();
    ix.accounts[1].is_signer = false;
    assert_eq!(
        custom_error(&f.run(&ix)),
        Some(GachaError::NotSigner as u32)
    );
    let mut ix = good.clone();
    ix.accounts[7].pubkey = ata(&f.authority, &f.payment_mint);
    assert_eq!(
        custom_error(&f.run(&ix)),
        Some(GachaError::InvalidTokenAddress as u32)
    );

    f.mollusk.sysvars.clock.unix_timestamp = quote.expires_at;
    assert_eq!(
        custom_error(&f.run(&good)),
        Some(GachaError::QuoteExpired as u32)
    );
    f.mollusk.sysvars.clock.unix_timestamp -= 1;
    assert!(f.run(&good).program_result.is_ok());
    let retire = f.client_pool().set_status(PoolStatus::Retired).unwrap();
    assert!(f.run(&retire).program_result.is_ok());
    assert_eq!(
        custom_error(&f.run(&good)),
        Some(GachaError::InvalidPoolStatus as u32)
    );
}

#[test]
fn insufficient_surplus_rolls_back_the_return_restock_and_account_creation() {
    let (mut f, quote, signature) = setup();
    assert!(f.buy(1, [1; 32]).1.program_result.is_ok());
    let reserved = PRICE + BOND_PER_DRAW;
    f.upsert(
        f.vault,
        token_account(&f.payment_mint, &f.pool, reserved + quote.price - 1),
    );
    let payer = f.operator;
    let destination = ata(&f.buyer, &f.payment_mint);
    f.upsert(destination, wallet(0));
    let ix = instructions(&f, &quote, &signature);
    for instruction in &ix {
        f.ensure(instruction);
    }
    let keys = [
        f.pool,
        f.vault,
        destination,
        quote.asset,
        ix.last().unwrap().accounts[3].pubkey,
    ];
    let before = keys.map(|key| (key, f.account(&key).clone()));
    let result = f.run_transaction(&ix, &payer);
    assert!(result.raw_result.is_err());
    for (key, account) in before {
        assert_eq!(result.get_account(&key), Some(&account));
    }
    // Exactly the surplus succeeds while the accepted purchase stays fully funded.
    f.upsert(
        f.vault,
        token_account(&f.payment_mint, &f.pool, reserved + quote.price),
    );
    assert!(f.run_transaction(&ix, &payer).raw_result.is_ok());
    assert_eq!(token_amount(f.account(&f.vault)), reserved);
}
