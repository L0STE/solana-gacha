import type { GetMultipleAccountsApi, Rpc } from '@solana/rpc';
import type { Address } from '@solana/addresses';
import type { Instruction } from '@solana/instructions';
import type { Proof } from '@blueshift-gg/solana-ecvrf';
import { GachaClientError, Item, Pool, Pull, POOL_HEADER_LEN } from './index.js';
import type { Buy, Purchase } from './index.js';

/** Read-only RPC convenience. No sends, cached state, polling, or seed retries.
 * Uses confirmed state by default; pass finalized if the app requires it. */
export class Client {
  constructor(
    private readonly rpc: Rpc<GetMultipleAccountsApi>,
    private readonly commitment: 'confirmed' | 'finalized' = 'confirmed',
  ) {}

  async fetchPool(key: Address): Promise<Pool> {
    const { accounts } = await this.#read([key]);
    return Pool.fromAccount(key, accounts[0]!.owner, accounts[0]!.data);
  }
  async fetchPull(key: Address): Promise<Pull> {
    const { accounts } = await this.#read([key]);
    return Pull.fromAccount(key, accounts[0]!.owner, accounts[0]!.data);
  }
  async buy(key: Address, { buyer, count, clientSeed }: Buy): Promise<Purchase> {
    // Own the seed before the first await, including when it is a Buffer.
    const seed = Uint8Array.from(clientSeed);
    const { accounts } = await this.#read([key], undefined, POOL_HEADER_LEN);
    const pool = await Pool.fromAccount(key, accounts[0]!.owner, accounts[0]!.data);
    return pool.buy({ buyer, count, clientSeed: seed });
  }
  /** Requires the buyer's payment ATA; prepend native ATA CreateIdempotent if needed. */
  async refund(key: Address): Promise<Instruction> {
    const { pool, pull } = await this.#snapshot(key, false);
    return pool.refund(pull);
  }
  /** Resume recorded outcomes without a proof or an inventory read. */
  async deliver(key: Address, payer: Address): Promise<Instruction[][]> {
    return (await this.fetchPull(key)).deliver(payer);
  }
  async settle(key: Address, proof: Proof, payer: Address): Promise<Instruction[][]> {
    const { pool, pull, slot } = await this.#snapshot(key, true);
    const plan = await pool.settle(pull, proof);
    const keys = plan.draws.map(d => d.item);
    const { accounts } = await this.#read(keys, slot);
    const items = await Promise.all(accounts.map((account, i) =>
      Item.fromAccount(keys[i]!, account.owner, account.data),
    ));
    return plan.instructions(items, payer);
  }
  async #snapshot(key: Address, inventory: boolean): Promise<{ pool: Pool; pull: Pull; slot: bigint }> {
    const discovery = await this.#read([key]);
    const first = await Pull.fromAccount(key, discovery.accounts[0]!.owner, discovery.accounts[0]!.data);
    // One coherent read. Refunds need the complete 450-byte Pull but only
    // the Pool header, regardless of how much historical inventory exists.
    const { slot, accounts } = await this.#read([first.pool, key], discovery.slot, inventory ? undefined : 450);
    const poolAccount = accounts[0]!;
    const poolData = inventory ? poolAccount.data : poolAccount.data.subarray(0, POOL_HEADER_LEN);
    const [pool, pull] = await Promise.all([
      Pool.fromAccount(first.pool, poolAccount.owner, poolData),
      Pull.fromAccount(key, accounts[1]!.owner, accounts[1]!.data),
    ]);
    return { pool, pull, slot };
  }
  async #read(keys: readonly Address[], minimum?: bigint, length?: number) {
    const result = await this.rpc.getMultipleAccounts(keys, {
      commitment: this.commitment,
      encoding: 'base64',
      ...(minimum === undefined ? {} : { minContextSlot: minimum }),
      ...(length === undefined ? {} : { dataSlice: { offset: 0, length } }),
    }).send();
    if (result.value.length !== keys.length || (minimum !== undefined && result.context.slot < minimum)) {
      throw new GachaClientError('InvalidAccount');
    }
    const accounts = result.value.map(account => {
      if (!account) throw new GachaClientError('MissingAccount');
      return {
        owner: account.owner,
        data: Uint8Array.from(atob(account.data[0]), c => c.charCodeAt(0)),
      };
    });
    return { slot: result.context.slot, accounts };
  }
}
