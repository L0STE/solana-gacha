import { Proof, PublicKey as VrfPublicKey } from '@blueshift-gg/solana-ecvrf';
import { sha256 } from '@noble/hashes/sha2.js';
import { address, getAddressDecoder, getAddressEncoder, getProgramDerivedAddress } from '@solana/addresses';
import type { Address } from '@solana/addresses';
import { AccountRole } from '@solana/instructions';
import type { AccountMeta, Instruction } from '@solana/instructions';
import { compileTransactionMessage, getCompiledTransactionMessageEncoder } from '@solana/transaction-messages';

export type { Address, Instruction };
export { Client } from './rpc.js';
export const PROGRAM_ID = address('4X8u1YspRi6Z9TkZNb8qxNdwPLs5vDi7VRC2DhTheeKp');
export const CORE_PROGRAM_ID = address('CoREENxT6tW1HoK8ypY1SxRMZTcVPm7R94rH4PZNhX7d');
export const POOL_HEADER_LEN = 256;
/** Every instruction ends with this PDA and the program; events are emitted through them. */
export const EVENT_AUTHORITY = address('DzGCFfQ4o9bvpN3mhNmibxnf52DxBh7m7Ym8mbNqfpea');
const TOKEN = address('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');
const SYSTEM = address('11111111111111111111111111111111');
const ATA = address('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL');
const MAX_U64 = (1n << 64n) - 1n;
const POOL_STATUSES = ['paused', 'active', 'retired'] as const;
export type PoolStatus = typeof POOL_STATUSES[number];
const encoder = getAddressEncoder();
const decoder = getAddressDecoder();

export type ErrorCode = 'InvalidAccount' | 'InvalidArgument' | 'InvalidProof' | 'WrongPool'
  | 'NotPending' | 'NotSettled' | 'NotNextInQueue' | 'MissingInventory'
  | 'InvalidItem' | 'InvalidAsset' | 'InvalidPoolStatus' | 'PendingPurchases' | 'TransactionTooLarge' | 'MissingAccount';
export class GachaClientError extends Error {
  constructor(readonly code: ErrorCode) {
    super(code);
    this.name = 'GachaClientError';
  }
}
/** On-chain custom error codes, matching the Rust GachaError enum. */
export enum GachaError {
  NotMutable, NotSigner, InvalidAccountOwner, InvalidAccountLength, InvalidVersion,
  InvalidPoolParams, InvalidAuthority, InvalidOperator, InvalidTokenAddress,
  InvalidTier, InvalidItem, InvalidCount, NotNextInQueue, InvalidPullStatus,
  InvalidProof, SoldOut, DeadlineNotReached, InvalidOutcome, InsufficientBalance,
  InvalidBuyer, DeadlinePassed, InventoryChanged, InvalidQuote, QuoteExpired, InvalidAsset,
  InvalidPoolStatus, PendingPurchases, InvalidSeeds, AlreadyInitialized, PoolMismatch, InvalidItemCount, InvalidEventAuthority,
}
export interface Tier {
  readonly weight: number;
  readonly remaining: number;
}
/** A null tier means the outcome has already been delivered. */
export interface Outcome {
  readonly asset: Address;
  readonly tier: number | null;
}
export interface Draw {
  readonly tier: number;
  readonly position: number;
  readonly item: Address;
}
/** Core ownership and collection header. Core validates full state on transfer. */
export class Asset {
  private constructor(readonly address: Address, readonly owner: Address, readonly collection: Address | null) { Object.freeze(this); }
  static fromAccount(key: Address, owner: Address, data: Uint8Array): Asset {
    if (owner !== CORE_PROGRAM_ID || data.length < 34 || data[0] !== 1) fail('InvalidAsset');
    const kind = data[33]!;
    if (kind > 2 || (kind !== 0 && data.length < 66)) fail('InvalidAsset');
    return new Asset(address(key), keyAt(data, 1), kind === 2 ? keyAt(data, 34) : null);
  }
}

export interface Buy {
  readonly buyer: Address;
  readonly count: number;
  readonly clientSeed: Uint8Array;
}
/** Reusable offer from the pool authority. Expiry is a Unix timestamp in seconds. */
export interface BuybackQuote {
  readonly pool: Address;
  readonly asset: Address;
  readonly price: bigint;
  readonly expiresAt: bigint;
  readonly tier: number;
}

/** Sign these bytes with the pool authority's Ed25519 key. */
export function buybackMessage(quote: BuybackQuote): Uint8Array {
  if (!integer(quote.tier, 0, 7) || quote.price <= 0n || quote.expiresAt <= 0n || quote.expiresAt > (1n << 63n) - 1n) fail('InvalidArgument');
  return concat(new TextEncoder().encode('gacha:buyback:v1'), keyBytes(PROGRAM_ID),
    keyBytes(quote.pool), keyBytes(quote.asset), u64(quote.price), u64(quote.expiresAt), Uint8Array.of(quote.tier));
}
/** Track pull after confirming instruction. Preparing is not buying. */
export interface Purchase {
  readonly pull: Address;
  readonly instruction: Instruction;
}
export interface CreatePool {
  readonly id: bigint;
  readonly price: bigint;
  readonly deadlineSlots: bigint;
  readonly bondPerDraw: bigint;
  readonly bond: bigint;
  readonly weights: readonly number[];
}

/** Create the vault and paused pool atomically, then fund the vault with `bond`
 * from the authority's payment ATA. Authority pays ATA and pool rent. */
export async function createPoolInstructions(
  config: CreatePool,
  authority: Address,
  operator: Address,
  paymentMint: Address,
): Promise<Instruction[]> {
  const { id, price, deadlineSlots, bondPerDraw, bond } = config;
  const weights = [...config.weights];
  const numbers = [id, price, deadlineSlots, bondPerDraw].map(u64);
  if (
    !integer(weights.length, 1, 8)
    || weights.some(w => !integer(w, 1, 0xffffffff))
    || price === 0n
    || deadlineSlots === 0n
    || bondPerDraw === 0n
    || (price + bondPerDraw) * 10n > MAX_U64
    || !VrfPublicKey.from(operator).isValid()
  ) {
    fail('InvalidArgument');
  }

  const [pool, bump] = await Pool.addressFor(authority, id);
  const data = concat(
    Uint8Array.of(0),
    ...numbers,
    Uint8Array.of(weights.length),
    ...Array.from({ length: 8 }, (_, i) => u32(weights[i] ?? 0)),
    Uint8Array.of(bump),
  );
  const vault = await ata(pool, paymentMint);
  const accounts = [rw(authority, true), rw(pool), ro(operator), ro(paymentMint), ro(vault), ro(SYSTEM)];
  const instructions = [await createAta(authority, pool, paymentMint), ix(data, accounts)];
  if (bond > 0n) instructions.push(tokenTransfer(await ata(authority, paymentMint), vault, authority, bond));
  return instructions;
}

/** Immutable account snapshot. RPC reads and signing stay with the caller. */
export class Pool {
  readonly #data: Uint8Array;
  readonly #address: Address;
  private constructor(key: Address, data: Uint8Array) {
    this.#address = key;
    this.#data = Uint8Array.from(data);
  }

  /** Address and bump; the bump travels in CreatePool's instruction data. */
  static async addressFor(authority: Address, id: bigint): Promise<[Address, number]> { return pda('pool', keyBytes(authority), u64(id)); }

  /** The first 256 bytes suffice for purchases/admin; settlement needs full data.
   * Checks owner/layout/PDA, but cannot authenticate the RPC provider itself. */
  static async fromAccount(key: Address, owner: Address, data: Uint8Array): Promise<Pool> {
    check(owner, data, POOL_HEADER_LEN);
    const pool = new Pool(address(key), data);
    const tierCount = pool.#data[2]!;
    const length = pool.#data.length;
    if (
      !integer(tierCount, 1, 8)
      || pool.#data[3]! >= POOL_STATUSES.length
      || (length !== POOL_HEADER_LEN && length !== POOL_HEADER_LEN + inventorySpace(pool.inventoryVersion))
      || pool.price === 0n
      || pool.bondPerDraw === 0n
      || pool.deadlineSlots === 0n
      || (pool.price + pool.bondPerDraw) * 10n > MAX_U64
      || pool.tiers.some(t => t.weight === 0)
    ) {
      fail('InvalidAccount');
    }
    if (length > POOL_HEADER_LEN) {
      const counts = Array<number>(tierCount).fill(0);
      for (let position = 0; position < pool.inventoryVersion; position++) {
        const tag = pool.#tag(position);
        if (tag > tierCount) fail('InvalidAccount');
        if (tag > 0) counts[tag - 1]!++;
      }
      if (pool.tiers.some((t, i) => t.remaining !== counts[i])) fail('InvalidAccount');
    }
    const [derived, bump] = await Pool.addressFor(pool.authority, pool.id);
    if (derived !== key || bump !== pool.#data[1] || await ata(key, pool.paymentMint) !== pool.vault) {
      fail('InvalidAccount');
    }
    Object.freeze(pool);
    return pool;
  }

  get address(): Address { return this.#address; }
  get authority(): Address { return keyAt(this.#data, 8); }
  get operator(): Address { return keyAt(this.#data, 40); }
  get paymentMint(): Address { return keyAt(this.#data, 72); }
  get vault(): Address { return keyAt(this.#data, 104); }
  get id(): bigint { return readU64(this.#data, 136); }
  get price(): bigint { return readU64(this.#data, 144); }
  get deadlineSlots(): bigint { return readU64(this.#data, 152); }
  get bondPerDraw(): bigint { return readU64(this.#data, 160); }
  get pendingDraws(): bigint { return readU64(this.#data, 168); }
  get nextIndex(): bigint { return readU64(this.#data, 176); }
  get nextSettle(): bigint { return readU64(this.#data, 184); }
  get inventoryVersion(): number { return readU32(this.#data, 4); }
  get status(): PoolStatus { return POOL_STATUSES[this.#data[3]!]!; }

  /** Authority controls admissions. Retirement is terminal; buyer exits stay open. */
  setStatus(status: PoolStatus): Instruction {
    const tag = POOL_STATUSES.indexOf(status);
    if (tag < 0) fail('InvalidArgument');
    if (this.status === 'retired' && status !== 'retired') fail('InvalidPoolStatus');
    return ix(Uint8Array.of(3, tag), [ro(this.authority, true), rw(this.address)]);
  }
  get tiers(): readonly Tier[] {
    const tiers = Array.from({ length: this.#data[2]! }, (_, i) => Object.freeze({
      weight: readU32(this.#data, 192 + 8 * i),
      remaining: readU32(this.#data, 196 + 8 * i),
    }));
    return Object.freeze(tiers);
  }
  /** Surplus after pending refunds. Supply this pool's current vault balance
   * in raw payment-token units; execution rechecks the balance. */
  spendableBalance(vaultBalance: bigint): bigint {
    if (vaultBalance < 0n || vaultBalance > MAX_U64) fail('InvalidArgument');
    const reserved = this.pendingDraws * (this.price + this.bondPerDraw);
    if (reserved > MAX_U64) fail('InvalidAccount');
    return vaultBalance > reserved ? vaultBalance - reserved : 0n;
  }
  /** Maximum draws given stock and collateral; zero unless the pool is active.
   * Buyer funds, token restrictions and later state changes can still block it. */
  availableDraws(vaultBalance: bigint): number {
    if (this.status !== 'active') return 0;
    const stock = this.tiers.reduce((sum, tier) => sum + BigInt(tier.remaining), 0n) - this.pendingDraws;
    if (stock < 0n) fail('InvalidAccount');
    const funded = this.spendableBalance(vaultBalance) / this.bondPerDraw;
    return Math.min(10, Number(stock), Number(funded));
  }
  #tag(position: number): number {
    const block = Math.floor(position / 64);
    return this.#data[POOL_HEADER_LEN + block * 96 + 32 + position % 64]!;
  }

  /** Use a fresh random seed. Refetch and replace it if this snapshot goes stale. */
  async buy({ buyer, count, clientSeed }: Buy): Promise<Purchase> {
    if (this.status !== 'active') fail('InvalidPoolStatus');
    if (!integer(count, 1, 10)) fail('InvalidArgument');
    const seed = bytes(clientSeed, 32); // own the seed before the first await
    const [pull, bump] = await Pull.addressFor(this.address, seed);
    const data = concat(Uint8Array.of(10, count), seed, u32(this.inventoryVersion), Uint8Array.of(bump));
    const accounts = [
      rw(buyer, true),
      rw(this.address),
      rw(pull),
      rw(await ata(buyer, this.paymentMint)),
      rw(this.vault),
      ro(TOKEN),
      ro(SYSTEM),
    ];
    return { pull, instruction: ix(data, accounts) };
  }

  /** Deposit an owned Core asset; the snapshot supplies its collection. */
  async deposit(tier: number, asset: Asset): Promise<Instruction> {
    if (this.status === 'retired') fail('InvalidPoolStatus');
    if (!integer(tier, 0, this.#data[2]! - 1)) fail('InvalidArgument');
    if (asset.owner !== this.authority) fail('InvalidAsset');
    const [item, bump] = await Item.addressFor(this.address, tier, this.inventoryVersion);
    return ix(Uint8Array.of(1, tier, bump), [
      rw(this.authority, true), rw(this.address), rw(item),
      rw(asset.address), ro(asset.collection ?? CORE_PROGRAM_ID), ro(CORE_PROGRAM_ID), ro(SYSTEM),
    ]);
  }

  /** Prepare atomic return/payment/restock, with payer funding account rent.
   * Refetch and rebuild on an inventory race; the quote remains usable. */
  async buyback(quote: BuybackQuote, signature: Uint8Array, asset: Asset, payer: Address = asset.owner): Promise<Instruction[]> {
    if (this.status !== 'active') fail('InvalidPoolStatus');
    const { pool, tier } = quote;
    if (pool !== this.address) fail('WrongPool');
    if (!integer(tier, 0, this.#data[2]! - 1)) fail('InvalidArgument');
    if (quote.asset !== asset.address || asset.owner === pool) fail('InvalidAsset');
    const message = buybackMessage(quote);
    const [item, bump] = await Item.addressFor(pool, tier, this.inventoryVersion);
    const data = concat(Uint8Array.of(12, tier), message.slice(112, 128), bytes(signature, 64), Uint8Array.of(bump));
    const accounts = [
      rw(payer, true), ro(asset.owner, true), rw(pool), rw(item),
      rw(asset.address), ro(asset.collection ?? CORE_PROGRAM_ID),
      rw(this.vault), rw(await ata(asset.owner, this.paymentMint)),
      ro(CORE_PROGRAM_ID), ro(TOKEN), ro(SYSTEM),
    ];
    return [await createAta(payer, asset.owner, this.paymentMint), ix(data, accounts)];
  }

  /** Return an unsold asset and its Item rent to the authority after retirement.
   * Pending purchases must resolve first; awarded Items cannot be reclaimed. */
  reclaim(item: Item, asset: Asset): Instruction {
    if (this.status !== 'retired') fail('InvalidPoolStatus');
    if (this.pendingDraws !== 0n) fail('PendingPurchases');
    if (item.pool !== this.address || item.tier >= this.#data[2]!
      || item.position >= this.inventoryVersion || item.asset !== asset.address) fail('InvalidItem');
    if (asset.owner !== this.address) fail('InvalidAsset');
    return ix(Uint8Array.of(4), [rw(this.authority, true), rw(this.address), rw(item.address),
      rw(asset.address), ro(asset.collection ?? CORE_PROGRAM_ID), ro(CORE_PROGRAM_ID), ro(SYSTEM)]);
  }

  withdraw(destination: Address, amount: bigint): Instruction {
    const data = concat(Uint8Array.of(2), u64(amount));
    const accounts = [
      ro(this.authority, true),
      ro(this.address),
      rw(this.vault),
      rw(destination),
      ro(TOKEN),
    ];
    return ix(data, accounts);
  }

  /** Recreate the buyer's payment ATA if needed and refund atomically.
   * Only payer signs; the chain enforces the deadline and collateral. */
  async refund(pull: Pull, payer: Address): Promise<Instruction[]> {
    this.#pending(pull);
    const accounts = [
      rw(this.address),
      rw(pull.address),
      rw(pull.buyer),
      rw(this.vault),
      rw(await ata(pull.buyer, this.paymentMint)),
      ro(TOKEN),
    ];
    return [await createAta(payer, pull.buyer, this.paymentMint), ix(Uint8Array.of(11), accounts)];
  }

  #pending(pull: Pull): void {
    if (pull.pool !== this.address) fail('WrongPool');
    if (pull.status !== 'pending') fail('NotPending');
    if (pull.index !== this.nextSettle) fail('NotNextInQueue');
    if (pull.inventoryVersion > this.inventoryVersion) fail('InvalidAccount');
  }

  /** Verify the proof before deriving the selected item accounts. */
  async settle(pull: Pull, proof: Proof): Promise<Settlement> {
    this.#pending(pull);
    if (this.#data.length !== POOL_HEADER_LEN + inventorySpace(this.inventoryVersion)) {
      fail('MissingInventory');
    }
    let beta: Uint8Array;
    try {
      beta = proof.verify(VrfPublicKey.from(this.operator), pull.alpha());
    } catch {
      fail('InvalidProof');
    }
    // ponytail: linear host scan; use indexed rank queries if large pools make preparation expensive.
    const candidates = this.tiers.map(() => [] as number[]);
    for (let position = 0; position < pull.inventoryVersion; position++) {
      const tag = this.#tag(position);
      if (tag > 0) candidates[tag - 1]!.push(position);
    }
    const weights = this.tiers.map(t => BigInt(t.weight));
    const selected: { tier: number; position: number }[] = [];
    for (let i = 0; i < pull.count; i++) {
      const hash = sha256(concat(beta, Uint8Array.of(i)));
      const total = weights.reduce((sum, w, t) => sum + (candidates[t]!.length ? w : 0n), 0n);
      if (total === 0n) fail('InvalidAccount');
      let roll = readU64(hash, 0) % total;
      let tier = 0;
      for (; tier < candidates.length; tier++) {
        if (candidates[tier]!.length === 0) continue;
        if (roll < weights[tier]!) break;
        roll -= weights[tier]!;
      }
      const list = candidates[tier]!;
      const rank = Number(readU64(hash, 8) % BigInt(list.length));
      selected.push({ tier, position: list.splice(rank, 1)[0]! });
    }
    const draws = await Promise.all(selected.map(async draw => Object.freeze({
      ...draw,
      item: (await Item.addressFor(this.address, draw.tier, draw.position))[0],
    })));
    return new Settlement(this.address, pull.address, pull.buyer, this.operator, proof, draws);
  }
}

export class Pull {
  readonly #data: Uint8Array;
  readonly #address: Address;
  private constructor(key: Address, data: Uint8Array) {
    this.#address = key;
    this.#data = Uint8Array.from(data);
  }
  /** Address and bump; the bump travels in Buy's instruction data. The FIFO
   * index is assigned on execution and recorded in the account. */
  static async addressFor(pool: Address, clientSeed: Uint8Array): Promise<[Address, number]> { return pda('pull', keyBytes(pool), bytes(clientSeed, 32)); }
  static async fromAccount(key: Address, owner: Address, data: Uint8Array): Promise<Pull> {
    check(owner, data, 450);
    const pull = new Pull(address(key), data);
    const d = pull.#data;
    if (d.length !== 450 || d[2]! > 1 || !integer(d[3]!, 1, 10)) fail('InvalidAccount');
    if (pull.status === 'settled') {
      for (let i = 0; i < pull.count; i++) {
        const tier = d[120 + i * 33]!;
        if (tier !== 255 && tier >= 8) fail('InvalidAccount');
      }
    }
    const [derived, bump] = await Pull.addressFor(pull.pool, pull.clientSeed);
    if (derived !== key || bump !== d[1]) fail('InvalidAccount');
    Object.freeze(pull);
    return pull;
  }
  get address(): Address { return this.#address; }
  get pool(): Address { return keyAt(this.#data, 24); }
  get buyer(): Address { return keyAt(this.#data, 56); }
  get count(): number { return this.#data[3]!; }
  get inventoryVersion(): number { return readU32(this.#data, 4); }
  get index(): bigint { return readU64(this.#data, 8); }
  get deadlineSlot(): bigint { return readU64(this.#data, 16); }
  get status(): 'pending' | 'settled' { return this.#data[2] === 0 ? 'pending' : 'settled'; }
  get clientSeed(): Uint8Array { return this.#data.slice(88, 120); }
  alpha(): Uint8Array { return sha256(concat(keyBytes(this.address), this.clientSeed)); }
  outcomes(): readonly Outcome[] {
    if (this.status !== 'settled') fail('NotSettled');
    const outcomes = Array.from({ length: this.count }, (_, i) => {
      const offset = 120 + i * 33;
      const tier = this.#data[offset]!;
      return Object.freeze({
        tier: tier === 255 ? null : tier,
        asset: keyAt(this.#data, offset + 1),
      });
    });
    return Object.freeze(outcomes);
  }
  /** Recovery only includes outcomes that remain undelivered. */
  async deliver(assets: readonly Asset[], payer: Address): Promise<Instruction[][]> {
    const pending = [...this.outcomes().entries()].filter(([, o]) => o.tier !== null);
    if (pending.length !== assets.length) fail('InvalidAsset');
    const instructions = pending.map(([i, outcome], n) => {
      const asset = assets[n]!;
      if (outcome.asset !== asset.address) fail('InvalidAsset');
      return deliver(this.pool, this.address, this.buyer, asset, i, payer);
    });
    return batches([], instructions, payer);
  }

}

export class Item {
  readonly #data: Uint8Array;
  readonly #address: Address;
  private constructor(key: Address, data: Uint8Array) {
    this.#address = key;
    this.#data = Uint8Array.from(data);
  }
  /** Address and bump; the bump travels in DepositItem's and Buyback's instruction data. */
  static async addressFor(pool: Address, tier: number, position: number): Promise<[Address, number]> {
    if (!integer(tier, 0, 7)) fail('InvalidArgument');
    return pda('item', keyBytes(pool), Uint8Array.of(tier), u32(position));
  }
  static async fromAccount(key: Address, owner: Address, data: Uint8Array): Promise<Item> {
    check(owner, data, 72);
    const item = new Item(address(key), data);
    if (item.#data.length !== 72 || item.tier >= 8) fail('InvalidAccount');
    const [derived, bump] = await Item.addressFor(item.pool, item.tier, item.position);
    if (derived !== key || bump !== item.#data[1]) {
      fail('InvalidAccount');
    }
    Object.freeze(item);
    return item;
  }
  get address(): Address { return this.#address; }
  get tier(): number { return this.#data[2]!; }
  get position(): number { return readU32(this.#data, 4); }
  get pool(): Address { return keyAt(this.#data, 8); }
  get asset(): Address { return keyAt(this.#data, 40); }
}

/** Only exported as a type: obtain a plan through Pool.settle, never raw beta. */
class Settlement {
  readonly #proof: Proof;
  readonly #draws: readonly Draw[];
  constructor(
    private readonly pool: Address,
    private readonly pull: Address,
    private readonly buyer: Address,
    private readonly operator: Address,
    proof: Proof,
    draws: readonly Draw[],
  ) {
    this.#proof = proof;
    this.#draws = Object.freeze([...draws]);
    Object.freeze(this);
  }
  get draws(): readonly Draw[] { return this.#draws; }
  instruction(): Instruction {
    const data = concat(Uint8Array.of(20), this.#proof.bytes);
    const accounts = [
      rw(this.operator),
      rw(this.pool),
      rw(this.pull),
      ...this.#draws.map(draw => rw(draw.item)),
    ];
    return ix(data, accounts);
  }
  async instructions(items: readonly Item[], assets: readonly Asset[], payer: Address): Promise<Instruction[][]> {
    if (items.length !== this.#draws.length || items.some((item, i) => item.address !== this.#draws[i]!.item)) fail('InvalidItem');
    if (assets.length !== items.length) fail('InvalidAsset');
    const instructions = items.map((item, i) => {
      const asset = assets[i]!;
      if (item.asset !== asset.address) fail('InvalidAsset');
      return deliver(this.pool, this.pull, this.buyer, asset, i, payer);
    });
    return batches([this.instruction()], instructions, payer);
  }

}
export type { Settlement };

function fail(code: ErrorCode): never { throw new GachaClientError(code); }
function inventorySpace(version: number): number { return Math.ceil(version / 64) * 96; }
function integer(n: number, min: number, max: number): boolean { return Number.isSafeInteger(n) && n >= min && n <= max; }
function check(owner: Address, data: Uint8Array, min: number): void {
  if (owner !== PROGRAM_ID || data.length < min || data[0] !== 1) fail('InvalidAccount');
}
function bytes(data: Uint8Array, length: number): Uint8Array {
  if (data.length !== length) fail('InvalidArgument');
  return Uint8Array.from(data);
}
function concat(...parts: Uint8Array[]): Uint8Array {
  const result = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.length;
  }
  return result;
}
function view(data: Uint8Array): DataView { return new DataView(data.buffer, data.byteOffset, data.byteLength); }
function readU64(data: Uint8Array, offset: number): bigint { return view(data).getBigUint64(offset, true); }
function readU32(data: Uint8Array, offset: number): number { return view(data).getUint32(offset, true); }
function u64(n: bigint): Uint8Array {
  if (typeof n !== 'bigint' || n < 0n || n > MAX_U64) fail('InvalidArgument');
  const data = new Uint8Array(8);
  view(data).setBigUint64(0, n, true);
  return data;
}
function u32(n: number): Uint8Array {
  if (!integer(n, 0, 0xffffffff)) fail('InvalidArgument');
  const data = new Uint8Array(4);
  view(data).setUint32(0, n, true);
  return data;
}
function keyAt(data: Uint8Array, offset: number): Address { return decoder.decode(data.subarray(offset, offset + 32)); }
function keyBytes(key: Address): Uint8Array { return Uint8Array.from(encoder.encode(key)); }
async function pda(seed: string, ...seeds: Uint8Array[]): Promise<[Address, number]> {
  const [key, bump] = await getProgramDerivedAddress({ programAddress: PROGRAM_ID, seeds: [seed, ...seeds] });
  return [key, bump];
}
async function ata(owner: Address, mint: Address): Promise<Address> {
  const [key] = await getProgramDerivedAddress({
    programAddress: ATA,
    seeds: [keyBytes(owner), keyBytes(TOKEN), keyBytes(mint)],
  });
  return key;
}
function rw(key: Address, signer = false): AccountMeta {
  const role = signer ? AccountRole.WRITABLE_SIGNER : AccountRole.WRITABLE;
  return { address: address(key), role };
}
function ro(key: Address, signer = false): AccountMeta {
  const role = signer ? AccountRole.READONLY_SIGNER : AccountRole.READONLY;
  return { address: address(key), role };
}
/** Every instruction ends with the event authority and the program, for the event CPI. */
function ix(data: Uint8Array, accounts: AccountMeta[]): Instruction { return { programAddress: PROGRAM_ID, data, accounts: [...accounts, ro(EVENT_AUTHORITY), ro(PROGRAM_ID)] }; }
function deliver(pool: Address, pull: Address, buyer: Address, asset: Asset, outcome: number, payer: Address): Instruction {
  if (asset.owner !== pool) fail('InvalidAsset');
  return ix(Uint8Array.of(21, outcome), [
    rw(payer, true), ro(pool), rw(pull), rw(buyer), rw(asset.address),
    ro(asset.collection ?? CORE_PROGRAM_ID), ro(CORE_PROGRAM_ID), ro(SYSTEM),
  ]);
}
/** SPL Token `Transfer`; collateral enters the vault as an ordinary transfer. */
function tokenTransfer(from: Address, to: Address, authority: Address, amount: bigint): Instruction {
  return { programAddress: TOKEN, data: concat(Uint8Array.of(3), u64(amount)), accounts: [rw(from), rw(to), ro(authority, true)] };
}
async function createAta(payer: Address, owner: Address, mint: Address): Promise<Instruction> {
  const accounts = [
    rw(payer, true),
    rw(await ata(owner, mint)),
    ro(owner),
    ro(mint),
    ro(SYSTEM),
    ro(TOKEN),
  ];
  return { programAddress: ATA, data: Uint8Array.of(1), accounts };
}
function transactionSize(instructions: readonly Instruction[], payer: Address): number {
  const message = compileTransactionMessage({ version: 'legacy', feePayer: { address: payer }, instructions });
  return 1 + message.header.numSignerAccounts * 64 + getCompiledTransactionMessageEncoder().encode(message).length;
}
function batches(initial: Instruction[], instructions: Instruction[], payer: Address): Instruction[][] {
  const groups = [initial];
  if (transactionSize(initial, payer) > 1232) fail('TransactionTooLarge');
  for (const instruction of instructions) {
    const current = groups.at(-1)!;
    current.push(instruction);
    if (transactionSize(current, payer) > 1232) {
      const next = current.splice(-1);
      if (transactionSize(next, payer) > 1232) fail('TransactionTooLarge');
      groups.push(next);
    }
  }
  return groups.filter(group => group.length > 0);
}
