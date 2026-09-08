//! CU benches; `cargo bench -p gacha-tests` regenerates `benches/compute_units.md`.

use ed25519_dalek::{Signer, SigningKey};
use gacha_rust::BuybackQuote;
use gacha_tests::*;
use mollusk_svm_bencher::MolluskComputeUnitBencher;

fn main() {
    let mut f = Fixture::new();
    f.open_pool();
    for tier in 0..3 {
        for _ in 0..4 {
            f.deposit(tier);
        }
    }
    let (pull1, _) = f.buy(1, [1; 32]);
    let (settle1, _) = f.settle_ix(&pull1, &mut f.model.clone());
    f.ensure(&settle1);
    let accounts1 = f.accounts.clone();
    // Settle the single pull so a ten-pull can be set up behind it.
    assert!(f.settle(&pull1).0.program_result.is_ok());
    let asset = f.client_pull(&pull1).outcomes().unwrap()[0].asset;
    let deliver = f
        .client_pull(&pull1)
        .deliver(&[f.client_asset(asset)], f.operator)
        .unwrap()
        .remove(0)
        .remove(0);
    f.ensure(&deliver);
    let accounts_deliver = f.accounts.clone();
    assert!(f.run(&deliver).program_result.is_ok());
    let quote = BuybackQuote {
        pool: f.pool,
        asset,
        price: PRICE,
        expires_at: 300,
        tier: 0,
    };
    let signature = SigningKey::from_bytes(&AUTHORITY_SEED)
        .sign(&quote.message())
        .to_bytes();
    let buyback = f
        .client_pool()
        .buyback(&quote, &signature, &f.client_asset(asset), f.operator)
        .unwrap()
        .pop()
        .unwrap();
    f.ensure(&buyback);
    let accounts_buyback = f.accounts.clone();
    let (pull10, _) = f.buy(10, [2; 32]);
    let (settle10, _) = f.settle_ix(&pull10, &mut f.model.clone());
    f.ensure(&settle10);
    let accounts10 = f.accounts.clone();
    let buy = f.buy_ix(1, [3; 32]);
    f.ensure(&buy);
    let accounts_buy = f.accounts.clone();

    // Exercise prefix counts and rank selection across blocks after restocking.
    assert!(f.settle(&pull10).0.program_result.is_ok());
    for position in 12..511 {
        assert!(f.deposit((position % 3) as u8).program_result.is_ok());
    }
    let (restocked_pull, result) = f.buy(10, [4; 32]);
    assert!(result.program_result.is_ok());
    for position in 511..576 {
        assert!(f.deposit((position % 3) as u8).program_result.is_ok());
    }
    let (settle_restocked, _) = f.settle_ix(&restocked_pull, &mut f.model.clone());
    f.ensure(&settle_restocked);
    let accounts_restocked = f.accounts.clone();

    MolluskComputeUnitBencher::new(f.mollusk)
        .bench(("buy", &buy, &accounts_buy))
        .bench(("deliver_core", &deliver, &accounts_deliver))
        .bench(("buyback_core", &buyback, &accounts_buyback))
        .bench(("settle_1", &settle1, &accounts1))
        .bench(("settle_10", &settle10, &accounts10))
        .bench((
            "settle_10_restocked_511",
            &settle_restocked,
            &accounts_restocked,
        ))
        .must_pass(true)
        .out_dir("benches")
        .execute();
}
