import type { GetMultipleAccountsApi, Rpc } from '@solana/rpc';
import type { Address } from '@solana/addresses';
import type { Instruction } from '@solana/instructions';
import type { Proof } from '@blueshift-gg/solana-ecvrf';
import { GachaClientError, Asset, Item, Pool, Pull, POOL_HEADER_LEN } from './index.js';
import type { Buy, BuybackQuote, Purchase } from './index.js';

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
  /** Fetch the pool and Core asset together, then prepare the complete buyback. */
  async buyback(quote: BuybackQuote, signature: Uint8Array, payer?: Address): Promise<Instruction[]> {
    const terms = { ...quote };
    const sig = Uint8Array.from(signature);
    const { accounts } = await this.#read([terms.pool, terms.asset], undefined, POOL_HEADER_LEN);
    const pool = await Pool.fromAccount(terms.pool, accounts[0]!.owner, accounts[0]!.data);
    const asset = Asset.fromAccount(terms.asset, accounts[1]!.owner, accounts[1]!.data);
    return pool.buyback(terms, sig, asset, payer ?? asset.owner);
  }
  async fetchAsset(key: Address): Promise<Asset> {
    return (await this.#assets([key]))[0]!;
  }
  async fetchItem(key: Address): Promise<Item> {
    const { accounts } = await this.#read([key]);
    return Item.fromAccount(key, accounts[0]!.owner, accounts[0]!.data);
  }
  /** Prepare payment ATA creation and refund together; payer funds any rent. */
  async refund(key: Address, payer: Address): Promise<Instruction[]> {
    const { pool, pull } = await this.#snapshot(key, false);
    return pool.refund(pull, payer);
  }
  /** Resume recorded outcomes, including their current Core collection accounts. */
  async deliver(key: Address, payer: Address): Promise<Instruction[][]> {
    const { slot, accounts } = await this.#read([key]);
    const pull = await Pull.fromAccount(key, accounts[0]!.owner, accounts[0]!.data);
    const keys = pull.outcomes().filter(o => o.tier !== null).map(o => o.asset);
    return pull.deliver(await this.#assets(keys, slot), payer);
  }
  async settle(key: Address, proof: Proof, payer: Address): Promise<Instruction[][]> {
    const { pool, pull, slot } = await this.#snapshot(key, true);
    const plan = await pool.settle(pull, proof);
    const keys = plan.draws.map(d => d.item);
    const { accounts, slot: itemSlot } = await this.#read(keys, slot);
    const items = await Promise.all(accounts.map((account, i) =>
      Item.fromAccount(keys[i]!, account.owner, account.data),
    ));
    return plan.instructions(items, await this.#assets(items.map(item => item.asset), itemSlot), payer);
  }
  async #assets(keys: readonly Address[], minimum?: bigint): Promise<Asset[]> {
    if (keys.length === 0) return [];
    const { accounts } = await this.#read(keys, minimum, 66);
    return accounts.map((account, i) => Asset.fromAccount(keys[i]!, account.owner, account.data));
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
