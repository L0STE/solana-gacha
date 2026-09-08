# Solana Gacha

A continuously stocked gacha for Solana: 1–10 draws per purchase, weighted prize
tiers, on-chain ECVRF verification, and funded timeout refunds. Prizes stay in
ordinary SPL token accounts until delivered to the buyer.

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

Settlement, buyer ATA creation, and delivery can share a transaction. Larger
purchases split into ordered transactions; recorded awards remain claimable if
delivery is interrupted. The final delivery closes the Pull account and returns
its rent to the buyer.

## This implementation

The program uses Pinocchio, no on-chain heap allocation, and native SPL custody.
The Rust and TypeScript clients derive accounts, validate snapshots, verify
proofs, and compose ordinary instructions. They do not hold wallet keys or send
transactions.

| State | Responsibility |
|---|---|
| Pool | Fixed price, tier weights, operator, deadlines, queue, availability index |
| Item | Immutable mint, tier, and global deposit position; closed at settlement |
| Pull | Buyer, committed seed/version, deadline, and recorded delivery progress |

### Restock without changing earlier purchases

Each deposit gets a permanent position and increments `inventory_version`.
A purchase is eligible only for positions below its signed version. New stock
immediately serves new purchases, even while older purchases are pending; it
never enters their candidate sets. A position's mint and tier cannot change,
and positions are never reused. A returned prize can be deposited again at a
new position; its mint stays the same.

Earlier FIFO winners still consume shared candidates, so a pinned candidate
list is not a promise that every candidate remains available. Empty eligible
tiers are excluded when drawing. Reservations prevent oversubscribing inventory.

The pool's availability index costs 96 bytes per 64 lifetime deposits. On-chain
rank selection uses a Fenwick index, without scanning historical winners or
requiring copied snapshot accounts. The index is append-only: pool rent and
account loading grow with its history; there is no compaction or pool-close
instruction. Host clients currently scan the index to prepare settlement.

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

Each prize is one raw unit of a legacy SPL token. The program does not validate
NFT supply, decimals, metadata, or mint/freeze authorities. Applications choose
which mints to admit; NFT prizes need those properties checked separately.

Delivery and refunds require transferable tokens. A retained freeze authority
can block prize delivery or payment-vault refunds; collateral does not bypass
SPL restrictions. Admit prizes with revoked freeze authority or explicitly trust
their issuer, and assess payment-token issuer controls too. A settled award has
no timeout refund, even if its token later becomes frozen.

## Tests and verification

Mollusk tests execute the compiled program, including collateral boundaries,
FIFO settlement, restocking, stale purchases, native ATA composition, and atomic
rollback. A Rust-generated flow pins TypeScript instructions, proof selection,
and packet boundaries to the same bytes. Both clients also test RPC reads and
recovery after partial delivery.

Requires Rust, Bun, cargo-build-sbf 4.x, and platform-tools v1.53 or newer.
The workspace consumes the published `solana-ecvrf` crate and
`@blueshift-gg/solana-ecvrf` npm package, both version 0.0.1.

```sh
cargo-build-sbf --manifest-path program/Cargo.toml --tools-version v1.53
RUST_LOG=error cargo test --workspace --features gacha-program/rpc -- --test-threads=1
cargo clippy --workspace --all-targets --features gacha-program/rpc -- -D warnings
cargo fmt --all -- --check
bun install --frozen-lockfile
bun run test
```

Recorded local compute measurements: Buy **5,492 CU**, one-draw settlement
**12,895 CU**, ten-draw settlement **18,968 CU**. A ten-draw purchase pinned at
511 deposit positions, with 65 later deposits, uses **25,348 CU**. These exclude
composed ATA creation/delivery; see [the benchmark log](tests/benches/compute_units.md).

The ECVRF dependency requires the feature-gated `sol_sha512` syscall. Check
activation on the deployment cluster. Passing local tests is not evidence of
mainnet compatibility or a production deployment.

## TypeScript: buy and track a pull

Import `@blueshift-gg/gacha` from this Bun workspace. It uses Solana Kit
instructions and accepts an existing RPC transport.

```ts
import { Client } from '@blueshift-gg/gacha';
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
the selected Item accounts at a context slot no older than that snapshot. The
proof can be published for any relayer to submit. `deliver` needs no proof or
inventory read; `refund(pullAddress)` builds the timeout refund instruction.
If the buyer closed their payment ATA, prepend the SPL Associated Token
program's `CreateIdempotent` instruction for `(buyer, paymentMint)`. Submit both
in one transaction: a relayer pays ATA rent, and the buyer need not sign. This
composition is safe when the ATA already exists and applies to both clients.

## Rust: the same flow

The host client lives in `gacha_program::client`. RPC is opt-in; omitting `rpc`
keeps the HTTP client and async runtime out of the core dependency graph.

```toml
[dependencies]
gacha-program = { path = "path/to/solana-gacha/program", features = ["rpc"] }
```

```rust
use gacha_program::client::{Buy, Client, RpcClient};

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

### Inventory management and offline use

Fetch a Pool with `fetchPool` / `fetch_pool`. Its `deposit(tier, mint)` returns
ATA creation plus deposit, ready to submit together; `withdraw(destination,
amount)` builds a surplus withdrawal to a token account. Create a pool with
`createPoolInstructions` in TypeScript or `CreatePool::instructions` in Rust;
both include vault creation. The authority must already hold the funding tokens
or prize in its corresponding ATA. Custody uses the legacy SPL Token program.

Applications with their own account source can use `Pool.fromAccount`,
`Pull.fromAccount`, and `Item.fromAccount` (`from_account` in Rust). These own
their bytes and check the owner, layout, and PDA; they do not authenticate an
untrusted RPC provider. `Pool.buy` works from the 256-byte header. Offline
settlement uses `Pool.settle` → fetch its `draws` → `Settlement.instructions`.
There is no unchecked randomness/output API.

### Transaction handling

`buy` prepares an instruction; it does not pay until the app submits it.
If another purchase or deposit makes it stale, resolve the old transaction's
status first, then refetch and use a **fresh seed** for a new attempt. Do not
share the seed with the operator before signing the purchase.

Settlement and delivery return ordered arrays of instructions, not signed
transactions. Each array fits a 1,232-byte legacy transaction with the supplied
fee payer. Add a fresh blockhash, sign, submit, and confirm before the next
batch. Adding instructions or signers requires another size check. Refetch before
recovery: previously delivered outcomes are skipped, and a fully delivered or
refunded Pull no longer exists. Use confirmed transaction history to distinguish
closure from an address that was never created.

Pool, Item, and Pull versions remain **1**. Instruction layouts live beside
their handlers; this development layout has no compatibility path for earlier
draft accounts.

## License

MIT. Review the implementation and operator trust model before using it to
secure value.
