# Solana Gacha Kit client

Typed snapshots, version-bound purchases, verified settlement, and Metaplex Core
NFT delivery and buyback instructions. Accepts an existing Solana Kit RPC; your app signs and
sends transactions.

```ts
import { Client } from '@blueshift-gg/gacha-kit';
import { createSolanaRpc } from '@solana/rpc';

const gacha = new Client(createSolanaRpc(rpcUrl));
const { pull, instruction } = await gacha.buy(poolAddress, {
  buyer: walletAddress,
  count: 1,
  clientSeed: crypto.getRandomValues(new Uint8Array(32)),
});
```

After submitting and confirming `instruction`, track `pull` with `fetchPull`.
`settle(pull, proof, payer)` prepares ordered settlement/delivery batches;
`deliver(pull, payer)` resumes remaining deliveries; `refund(pull, payer)` prepares
payment ATA creation and the timeout refund together. The payer covers any rent;
the buyer need not sign. No method sends transactions or retries purchases.

`buyback(quote, signature, payer?)` fetches the Core holder and collection and
prepares the atomic return/payment/restock, including the seller's payment ATA.
The authority signs `buybackMessage(quote)`; the current holder signs redemption.
`fetchAsset(address)` supplies the Core snapshot for `pool.deposit(tier, asset)`.

Pools start paused. After stocking, `pool.setStatus('active')` opens sales;
`'paused'` suspends them and `'retired'` ends them permanently. The authority
signs these instructions. Once retired with no pending purchases,
`pool.reclaim(item, asset)` returns an unsold NFT and Item rent to the authority.
Use `fetchItem` and `fetchAsset` for the snapshots. Existing purchases still
settle, deliver, or refund under their original rules.

`pool.availableDraws(vaultBalance)` calculates purchase capacity from stock and
collateral; `pool.spendableBalance(vaultBalance)` calculates surplus after refunds.
Supply the pool vault's balance in raw payment-token units. Both are snapshot
calculations; capacity is zero unless active and execution rechecks current state.

This package uses the published `@blueshift-gg/solana-ecvrf` package.
Run `bun install --frozen-lockfile && bun run test` from the repository root.
The root README covers operator setup, offline snapshots, and transaction safety.
