# Solana Gacha client

Typed snapshots, version-bound purchases, verified settlement, and native SPL
delivery instructions. Accepts an existing Solana Kit RPC; your app signs and
sends transactions.

```ts
import { Client } from '@blueshift-gg/gacha';
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
`deliver(pull, payer)` resumes remaining deliveries; `refund(pull)` prepares a
timeout refund. No method sends transactions or retries purchases.
Refund requires the buyer's payment ATA: prepend native ATA `CreateIdempotent`
if it may have been closed. The relayer can pay its rent without the buyer signing.

This package uses the published `@blueshift-gg/solana-ecvrf` package.
Run `bun install --frozen-lockfile && bun run test` from the repository root.
The root README covers operator setup, offline snapshots, and transaction safety.
