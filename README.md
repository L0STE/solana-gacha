# Solana Gacha

A continuously stocked Metaplex Core NFT gacha for Solana: 1–10 draws per purchase, weighted prize
tiers, on-chain ECVRF verification, and funded timeout refunds. The pool PDA owns each
Core asset until delivery; legacy SPL tokens handle payments and refunds.

## How it works

```text
Buy 1–10 draws → verify one proof and record the awards → deliver the prizes
             ↘ after the deadline: refund payment + penalty
```

A purchase escrows payment and reserves its draw count against available stock.
The buyer commits a fresh random seed and the current inventory version. The
operator produces a proof for `SHA-256(pull_address || client_seed)`. Anyone with
the proof can submit settlement; the operator does not need to sign it.

The program verifies the proof before deriving any draw. For each draw,
`SHA-256(verified_output || draw_index)` selects a nonempty eligible tier by its
relative weight, then a remaining item uniformly by rank. Purchases settle or
refund in FIFO order. Delivery always goes to the recorded buyer.

Settlement and Core delivery can share a transaction. The SDK splits batches
when needed to fit the transaction limit; recorded awards remain claimable if
delivery is interrupted. The final delivery closes the Pull account and returns
its rent to the buyer.

## This implementation

The program uses Pinocchio, no on-chain heap allocation, and native Core transfers.
The Rust and TypeScript clients derive accounts, validate snapshots, verify
proofs, and compose ordinary instructions. They do not hold wallet keys or send
transactions.

The [program](program), [Kit SDK](packages/gacha-kit), and
[Rust SDK](packages/gacha-rust) are separate packages. The program and Rust SDK
share layouts and draw rules through [gacha-core](packages/gacha-core), which
is `no_std` and allocation-free.

| State | Responsibility |
|---|---|
| Pool | Lifecycle, fixed price, tier weights, operator, deadlines, queue, availability index |
| Item | Immutable Core asset address, tier, and global deposit position; closed at settlement or reclamation |
| Pull | Buyer, committed seed/version, deadline, and recorded delivery progress |

### Open, pause, and retire

New pools start **paused** so the authority can stock them before opening sales.
Only the authority can change their status.

| Status | Buy / buyback | Deposit | Reclaim unsold NFTs |
|---|---|---|---|
| Paused | Blocked | Allowed | Blocked |
| Active | Allowed | Allowed | Blocked |
| Retired | Blocked | Blocked | After all pending purchases resolve |

Pausing is reversible; retirement is permanent. Settlement, delivery, timeout
refunds, and surplus withdrawals remain available in every state. Retirement
does not cancel purchases or release their refund reserves.

After retirement and an empty pending queue, the authority can reclaim each
unsold NFT and its Item rent in one instruction. Awarded NFTs remain reserved
for their buyers. The Pool stays alive for delivery, and its index rent remains
locked; reclaiming inventory does not compact or close it.

### Restock without changing earlier purchases

Each deposit gets a permanent position and increments `inventory_version`.
A purchase is eligible only for positions below its signed version. New stock
immediately serves new purchases, even while older purchases are pending; it
never enters their candidate sets. A position's asset and tier cannot change,
and positions are never reused. A returned prize can be deposited again at a
new position; its asset address stays the same.

Earlier FIFO winners still consume shared candidates, so a pinned candidate
list is not a promise that every candidate remains available. Empty eligible
tiers are excluded when drawing. Reservations prevent oversubscribing inventory.

The pool's availability index costs 96 bytes per 64 lifetime deposits. On-chain
rank selection uses a Fenwick index, without scanning historical winners or
requiring copied snapshot accounts. The index is append-only: pool rent and
account loading grow with its history; there is no compaction or pool-close
instruction. Host clients currently scan the index to prepare settlement.

### Buyback and restock

The pool authority can sign `{ pool, asset, price, expires_at, tier }` to offer a
buyback in the pool's payment token. The service sets expiry five minutes ahead;
the program checks the signed Unix timestamp against `Clock`. Quotes are open to
any holder and reusable until expiry. Every payout requires returning the prize.

Buyback transfers the prize, appends a fresh inventory position, and pays the
seller atomically. It spends only surplus above all pending refund obligations;
a quote does not reserve liquidity. No receipt accounts or quote IDs are created.
Brine verifies the authority's Ed25519 signature over the SDK's canonical message,
bound to this program and the `gacha:buyback:v1` domain. The SDK prepares the
seller's payment ATA and buyback together, with a separate rent payer when the
application sponsors the transaction. An inventory race requires
refetching and rebuilding the transaction, without obtaining a new signature.

### What verification does—and does not—guarantee

The proof binds one output to the operator key and committed input. It cannot
force the operator to publish that proof. The vault reserves
`pending_draws × (price + bond_per_draw)`; withdrawals take only surplus. After
the recorded deadline, settlement fails and anyone can refund the full payment
plus the configured penalty. Price, penalty, and deadline duration are fixed,
nonzero parameters. Funding the vault uses an ordinary SPL transfer.

The penalty is not compensation for an unseen jackpot. The operator can evaluate
its own candidate inputs and withhold earlier proofs, affecting what remains for
later pulls. Inventory versioning removes deposit steering, not these trust
limitations. An expired pull must also wait for earlier pulls to settle/refund.

There is no separate oracle fee or randomness-request account. Transaction fees,
account rent, proof generation, and RPC infrastructure still have costs.

### Asset assumptions

Prizes must be uncompressed Metaplex Core `AssetV1` accounts. Deposits and
buybacks require the current owner to authorize the transfer. Delivery transfers
ownership from the pool PDA to the recorded buyer. SPL and Token-2022 prizes,
Token Metadata NFTs, compressed assets, and collection accounts are not prizes.

Collection membership is read from the asset, and Core enforces its transfer
rules and applicable plugins. This integration uses standard `TransferV1`
accounts; external adapters requiring additional accounts are unsupported.

Core ownership alone does not remove issuer controls. Permanent transfer/burn
delegates and asset or collection freeze rules can remove custody or block
later delivery. The pool authority must assess those controls before admitting
an asset or signing a buyback. A settled award has no timeout refund. Payment
tokens also need transferable vaults: retained SPL freeze authority can block
refunds despite the reserved balance.

## Tests and verification

Mollusk tests execute the compiled gacha and published Core programs, covering
collateral boundaries, FIFO settlement, restocking, stale purchases, Core custody,
buybacks, retirement, unsold reclamation, payment ATA composition, and atomic rollback. A Rust-generated flow
pins TypeScript instructions, proof selection, and packet boundaries to the same
bytes. Both clients also test RPC reads and
recovery after partial delivery.

Requires Rust, Bun, cargo-build-sbf 4.x, and platform-tools v1.53 or newer.
The workspace consumes the published `solana-ecvrf` crate and
`@blueshift-gg/solana-ecvrf` npm package, both version 0.0.1.

```sh
cargo-build-sbf --manifest-path program/Cargo.toml --tools-version v1.53
curl -fL 'https://github.com/metaplex-foundation/mpl-core/releases/download/release/core%400.15.2/mpl_core_program.so' -o target/deploy/mpl_core_program.so
RUST_LOG=error cargo test --workspace -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
bun install --frozen-lockfile
bun run test
```

The Core test binary is release `core@0.15.2`; its SHA-256 is
`69be69672220f4b5b0813e53cc4e2f3042dcac991f088b174bc7907dab4b2dee`.

Recorded local compute measurements: Buy **5,537 CU**, one-draw settlement
**12,654 CU**, ten-draw settlement **18,169 CU**. A ten-draw purchase pinned at
511 deposit positions, with 65 later deposits, uses **25,983 CU**. Core delivery
uses **6,112 CU** and buyback **25,441 CU** for standalone assets without plugins.
These are individual instructions and exclude payment ATA creation; collection
and plugin rules add cost. See [the benchmark log](tests/benches/compute_units.md).

The ECVRF dependency requires the feature-gated `sol_sha512` syscall. Check
activation on the deployment cluster. Passing local tests is not evidence of
mainnet compatibility or a production deployment.

## TypeScript: buy and track a pull

Import `@blueshift-gg/gacha-kit` from this Bun workspace. It uses Solana Kit
instructions and accepts an existing RPC transport.

```ts
import { Client } from '@blueshift-gg/gacha-kit';
import { createSolanaRpc } from '@solana/rpc';

const gacha = new Client(createSolanaRpc(rpcUrl));
const { pull, instruction } = await gacha.buy(poolAddress, {
  buyer: walletAddress,
  count: 3,
  clientSeed: crypto.getRandomValues(new Uint8Array(32)),
});

// Your wallet signs/sends instruction. After confirmation:
const purchase = await gacha.fetchPull(pull);
```

All address arguments are Kit `Address` values; parse strings with `address()`
from `@solana/addresses`. Amounts, slots, and purchase indices use `bigint`.

### Operator: prove and fulfill

```ts
import { SecretKey } from '@blueshift-gg/solana-ecvrf';

const operator = SecretKey.fromKeypair(operatorKeypair); // backend only
const purchase = await gacha.fetchPull(pullAddress);
const proof = operator.prove(purchase.alpha());
const batches = await gacha.settle(pullAddress, proof, feePayerAddress);

// Sign, send, and confirm each instruction batch in order.
// If interrupted, refetch and build only outstanding deliveries:
const remaining = await gacha.deliver(pullAddress, feePayerAddress);
```

`settle` reads a coherent pool/pull snapshot, verifies the proof, and fetches only
the selected Item accounts, then their Core ownership/collection headers, at
context slots no older than each preceding read. The
proof can be published for any relayer to submit. `deliver` needs no proof or
inventory read. `refund(pullAddress, feePayerAddress)` prepares payment ATA
creation and the timeout refund together. Submit both instructions in one
transaction; the payer covers any account rent and the buyer need not sign.

## Rust: the same flow

The host client lives in `gacha_rust`. RPC is opt-in; omitting `rpc`
keeps the HTTP client and async runtime out of the core dependency graph.

```toml
[dependencies]
gacha-rust = { path = "path/to/solana-gacha/packages/gacha-rust", features = ["rpc"] }
```

```rust
use gacha_rust::{Buy, Client, RpcClient};

let gacha = Client::new(RpcClient::new(rpc_url));
let purchase = gacha.buy(pool_address, Buy {
    buyer: wallet_address,
    count: 3,
    client_seed: fresh_random_seed, // 32 bytes from the app's CSPRNG
}).await?;

// Sign/send purchase.instruction, then confirm before tracking purchase.pull.
let pull = gacha.fetch_pull(purchase.pull).await?;
let proof = operator.prove(&pull.alpha()); // solana_ecvrf::SecretKey, backend only
let batches = gacha.settle(pull.address(), &proof, fee_payer).await?;
// Resume partial delivery with gacha.deliver(pull.address(), fee_payer).
```

Use the ECVRF crate's `prove` feature in a Rust operator backend. The RPC wrapper
uses the supplied Rust transport's commitment and timeouts. TypeScript defaults
to `confirmed`; pass `'finalized'` as the second `Client` constructor argument
when required. Neither client caches state, polls, retries purchases, or submits
transactions automatically.

### Prepare a buyback

The backend signs `buybackMessage(quote)` in TypeScript or `quote.message()` in
Rust with the pool authority's Ed25519 key. `asset` is the Core asset address;
`price` is in raw payment-token units and `expiresAt` / `expires_at` is a Unix
timestamp in seconds, normally the backend's current time plus 300.

```ts
const instructions = await gacha.buyback(quote, signature, rentPayerAddress);
```

```rust
let instructions = gacha.buyback(&quote, &signature, rent_payer).await?;
```

Both clients fetch the current holder and collection automatically. The holder
signs the return and receives payment; the payer signs to fund account rent.
They can be the same wallet. Submit the returned instructions together.

### Inventory management and offline use

Fetch a Pool with `fetchPool` / `fetch_pool` and a Core Asset with `fetchAsset` /
`fetch_asset`. `pool.deposit(tier, asset)` returns one deposit instruction; the
asset supplies its owner and collection. `withdraw(destination, amount)` builds
a surplus withdrawal to a payment token account. Create a pool with
`createPoolInstructions` in TypeScript or `CreatePool::instructions` in Rust;
both include payment-vault creation and, for a nonzero `bond`, an ordinary SPL
transfer that funds it. The authority must own the Core prize and hold the
funding tokens in its payment ATA.

After stocking, submit `pool.setStatus('active')` in TypeScript or
`pool.set_status(PoolStatus::Active)?` in Rust. Use `paused` / `Paused` to suspend
sales and `retired` / `Retired` to end them permanently. Both require the authority's
signature. Once retirement is confirmed and `pendingDraws` / `pending_draws()`
is zero, fetch a live unsold Item to reclaim it:

```ts
const pool = await gacha.fetchPool(poolAddress);
const item = await gacha.fetchItem(itemAddress);
const asset = await gacha.fetchAsset(item.asset);
const instruction = pool.reclaim(item, asset);
```

```rust
let pool = gacha.fetch_pool(pool_address).await?;
let item = gacha.fetch_item(item_address).await?;
let asset = gacha.fetch_asset(item.asset()).await?;
let instruction = pool.reclaim(&item, &asset)?;
```

The authority signs reclamation and receives both the NFT and Item rent.

Given the pool vault's balance in raw payment-token units,
`pool.availableDraws(vaultBalance)` / `available_draws(vault_balance)` reports
how many draws one purchase can afford from current inventory and collateral,
capped at ten and zero unless active. `spendableBalance` / `spendable_balance` reports the surplus after
pending refunds, usable for buybacks or withdrawals. These use the supplied
snapshot and balance; buyer funds, token restrictions, and later state changes
can still prevent execution.

Applications with their own account source can use `Pool.fromAccount`,
`Pull.fromAccount`, and `Item.fromAccount` (`from_account` in Rust). These own
their bytes and check the owner, layout, and PDA; they do not authenticate an
untrusted RPC provider. `Pool.buy` works from the 256-byte header. Offline
settlement uses `Pool.settle` → fetch its drawn Items → fetch their Assets →
`Settlement.instructions(items, assets, payer)`. `Asset.fromAccount` reads only
the Core transfer header; full asset and plugin validation happens in Core.
There is no unchecked randomness/output API.

### Transaction handling

`buy` prepares an instruction; it does not pay until the app submits it.
The Pull address derives from the pool and the client seed, and the FIFO index
is assigned when the purchase lands, so concurrent buyers never contend for one
address. A deposit or buyback landing first still makes a prepared purchase
stale: resolve the old transaction's status, then refetch and use a **fresh
seed** for a new attempt. Do not share the seed with the operator before
signing the purchase.

### Events

Every instruction emits one event through a CPI to the program itself, signed
by the event authority PDA `[b"__event_authority"]`. The `Event` instruction
accepts only that signer, so events cannot be forged by other programs and
indexers read them from inner instructions rather than truncatable logs. Each
event is `[255, instruction discriminator, payload]`; Buy's payload carries the
Pull address and its FIFO index, since the queue position is not derivable
from the address alone. Every instruction therefore ends with two accounts:
the event authority and the program. Both SDKs append them.

Settlement and delivery return ordered arrays of instructions, not signed
transactions. Each array fits a 1,232-byte legacy transaction with the supplied
fee payer. Add a fresh blockhash, sign, submit, and confirm before the next
batch. Adding instructions or signers requires another size check. Refetch before
recovery: previously delivered outcomes are skipped, and a fully delivered or
refunded Pull no longer exists. Use confirmed transaction history to distinguish
closure from an address that was never created.

Instruction layouts and event payloads live beside their handlers. Each
handler deserializes its accounts and its instruction data separately, with
every check that needs only one of them living in that `TryFrom`; checks that
need both sit at the top of `process`, which then reads as one linear story.

## License

MIT. Review the implementation and operator trust model before using it to
secure value.
