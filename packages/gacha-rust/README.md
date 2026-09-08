# Solana Gacha Rust client

Metaplex Core NFT custody, buybacks, and verified settlement instructions. Your application
signs and sends transactions.

```toml
[dependencies]
gacha-rust = { path = "../solana-gacha/packages/gacha-rust", features = ["rpc"] }
```

```rust
use gacha_rust::{Buy, Client, RpcClient};

let gacha = Client::new(RpcClient::new(rpc_url));
let purchase = gacha.buy(pool_address, Buy {
    buyer: wallet_address,
    count: 1,
    client_seed: fresh_random_seed,
}).await?;
```

The `rpc` feature adds read-only RPC composition. Without it, use `Pool`, `Pull`,
`Item`, and Core `Asset` snapshots with your own account source. Neither mode submits transactions.

Pools start paused. After stocking, `pool.set_status(PoolStatus::Active)?` opens
sales; `Paused` suspends them and `Retired` ends them permanently. The authority
signs these instructions. Once retired with no pending purchases,
`pool.reclaim(&item, &asset)?` returns an unsold NFT and Item rent to the authority.
Use `fetch_item` and `fetch_asset` for the snapshots. Existing purchases still
settle, deliver, or refund under their original rules.

`buyback(&quote, &signature, payer)` fetches the Core holder and collection,
then prepares payment ATA creation and the atomic return/payment/restock. Sign
`quote.message()` with the pool authority; the current holder signs redemption.

`refund(pull, payer)` prepares payment ATA creation and refund together; only
the payer signs. `pool.available_draws(vault_balance)` and
`pool.spendable_balance(vault_balance)` calculate purchase capacity and surplus
from the snapshot and supplied pool vault balance in raw payment-token units.
Capacity is zero unless active. Execution rechecks current state.

See the [repository README](https://github.com/L0STE/solana-gacha) for operator
proofs, partial delivery recovery, refund ATA creation, and protocol assumptions.
