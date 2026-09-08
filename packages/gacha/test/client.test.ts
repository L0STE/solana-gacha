import { describe, expect, test } from 'bun:test';
import { Client, Pool, Pull, Item, createPoolInstructions, PROGRAM_ID } from '@blueshift-gg/gacha';
import type { Instruction } from '@blueshift-gg/gacha';
import { Proof, SecretKey } from '@blueshift-gg/solana-ecvrf';
import { address } from '@solana/addresses';
import { createSolanaRpcFromTransport } from '@solana/rpc';
import { sha256 } from '@noble/hashes/sha2.js';
import vector from '../../../program/src/client/test-vector.json' with { type: 'json' };

const bytes = (hex: string) => Buffer.from(hex, 'hex');
const buyer = address(vector.buyer);
const payer = address(vector.operator);
const proof = Proof.from(bytes(vector.proof));
type Account = typeof vector.pool;
const poolFrom = (a: Account) => Pool.fromAccount(address(a.address), address(a.owner), bytes(a.data));
const pullFrom = (a: Account) => Pull.fromAccount(address(a.address), address(a.owner), bytes(a.data));
const buy = () => ({ buyer, count: 10, clientSeed: new Uint8Array(32).fill(7) });
// Canonical field order matches serde_json's output in tests/tests/client.rs.
const wire = (ix: Instruction): typeof vector.buy => ({
  accounts: ix.accounts!.map(a => ({ address: a.address, role: a.role })),
  data: Buffer.from(ix.data!).toString('hex'), programAddress: ix.programAddress,
});
const fingerprint = (groups: Instruction[][]) => Buffer.from(sha256(new TextEncoder().encode(JSON.stringify(groups.map(g => g.map(wire)))))).toString('hex');

describe('Rust/on-chain parity', () => {
  test('create, stock, buy, verify, settle, and resume delivery', async () => {
    const create = await createPoolInstructions({ id: 1n, price: 5_000_000n, deadlineSlots: 100n,
      bondPerDraw: 1_000_000n, bond: 50_000_000n, weights: [79, 20, 1] }, address(vector.authority), payer, address(vector.paymentMint));
    expect(create.map(wire)).toEqual(vector.create);
    expect((await (await poolFrom(vector.beforeDeposit)).deposit(1, address(vector.depositMint))).map(wire)).toEqual(vector.deposit);
    const purchase = await (await poolFrom(vector.beforeBuy)).buy(buy());
    expect(purchase.pull).toBe(address(vector.pull.address));
    expect(wire(purchase.instruction)).toEqual(vector.buy);
    const pool = await poolFrom(vector.pool);
    const pull = await pullFrom(vector.pull);
    expect(pool.inventoryVersion).toBe(71);
    expect(pull.inventoryVersion).toBe(70);
    expect(SecretKey.fromSeed(new Uint8Array(32).fill(9)).prove(pull.alpha()).bytes).toEqual(proof.bytes);
    const plan = await pool.settle(pull, proof);
    expect(wire(plan.instruction())).toEqual(vector.settle);
    const items = await Promise.all(vector.items.map(a => Item.fromAccount(address(a.address), address(a.owner), bytes(a.data))));
    expect(fingerprint(await plan.instructions(items, payer))).toBe(vector.instructions);
    expect(wire(await pool.refund(pull))).toEqual(vector.refund);
    expect(wire(pool.withdraw(address(vector.withdraw.accounts[3]!.address), (1n << 64n) - 1n))).toEqual(vector.withdraw);
    expect(fingerprint(await (await pullFrom(vector.partialPull)).deliver(payer))).toBe(vector.recovery);
  });

  test('immutable snapshots own Buffer inputs before awaiting', async () => {
    const data = bytes(vector.pool.data);
    const pending = Pool.fromAccount(address(vector.pool.address), PROGRAM_ID, data);
    data.fill(0);
    const pool = await pending;
    expect(pool.inventoryVersion).toBe(71);
    const pull = await pullFrom(vector.pull);
    pull.clientSeed.fill(0);
    expect(pull.clientSeed).toEqual(new Uint8Array(32).fill(7));
    const params = buy();
    const purchase = (await poolFrom(vector.beforeBuy)).buy(params);
    params.clientSeed.fill(0);
    expect(wire((await purchase).instruction)).toEqual(vector.buy);
  });

  test('rejects malformed accounts and unverified settlement', async () => {
    const pool = await poolFrom(vector.pool);
    const pull = await pullFrom(vector.pull);
    await expect(Pool.fromAccount(pool.address, buyer, bytes(vector.pool.data))).rejects.toThrow('InvalidAccount');
    await expect(Pull.fromAccount(pull.address, PROGRAM_ID, bytes(vector.pull.data).subarray(0, 449))).rejects.toThrow('InvalidAccount');
    const header = await Pool.fromAccount(pool.address, PROGRAM_ID, bytes(vector.pool.data).subarray(0, 256));
    await expect(header.settle(pull, proof)).rejects.toThrow('MissingInventory');
    await expect(pool.settle(pull, SecretKey.fromSeed(new Uint8Array(32).fill(8)).prove(pull.alpha()))).rejects.toThrow('InvalidProof');
    await expect((await pool.settle(pull, proof)).instructions([], payer)).rejects.toThrow('InvalidItem');
    await expect(pull.deliver(payer)).rejects.toThrow('NotSettled');
    for (const count of [0, 11, 1.5, NaN]) await expect(pool.buy({ ...buy(), count })).rejects.toThrow('InvalidArgument');
    expect(() => pool.withdraw(buyer, -1n)).toThrow('InvalidArgument');
    expect(() => pool.withdraw(buyer, 1n << 64n)).toThrow('InvalidArgument');
  });
});

// Test through the official RPC transport, not a handwritten fake Client.
function mockRpc(responses: { slot: number; accounts: (Account | null)[] }[]) {
  const requests: { method: string; params: [string[], Record<string, unknown>] }[] = [];
  const rpc = createSolanaRpcFromTransport(async <TResponse>({ payload }: { payload: unknown }) => {
    const request = payload as { id: string; method: string; params: [string[], Record<string, unknown>] };
    requests.push(request);
    const next = responses.shift();
    if (!next) throw new Error('unexpected RPC request');
    const slice = request.params[1].dataSlice as { offset: number; length: number } | undefined;
    const value = next.accounts.map(account => {
      if (!account) return null;
      const fullData = bytes(account.data);
      const data = slice ? fullData.subarray(slice.offset, slice.offset + slice.length) : fullData;
      return {
        owner: account.owner, data: [data.toString('base64'), 'base64'],
        executable: false, lamports: 1, rentEpoch: 0, space: fullData.length,
      };
    });
    return { jsonrpc: '2.0', id: request.id, result: { context: { slot: next.slot }, value } } as TResponse;
  });
  return { client: new Client(rpc), requests };
}

test('RPC purchase uses a header read and snapshots its seed', async () => {
  const { client, requests } = mockRpc([{ slot: 10, accounts: [vector.beforeBuy] }]);
  const params = buy();
  const pending = client.buy(address(vector.pool.address), params);
  params.clientSeed.fill(0);
  expect(wire((await pending).instruction)).toEqual(vector.buy);
  expect(requests[0]!.method).toBe('getMultipleAccounts');
  expect(requests[0]!.params[1]).toMatchObject({ dataSlice: { offset: 0, length: 256 }, commitment: 'confirmed' });
});

test('RPC refund reads a bounded, coherent snapshot without decoding inventory', async () => {
  const pool = { ...vector.pool, data: vector.pool.data + '00'.repeat(4096) };
  const { client, requests } = mockRpc([
    { slot: 10, accounts: [vector.pull] },
    { slot: 11, accounts: [pool, vector.pull] },
  ]);
  expect(wire(await client.refund(address(vector.pull.address)))).toEqual(vector.refund);
  expect(requests).toHaveLength(2);
  expect(requests[1]!.params).toEqual([
    [vector.pool.address, vector.pull.address],
    { commitment: 'confirmed', encoding: 'base64', minContextSlot: 10n, dataSlice: { offset: 0, length: 450 } },
  ]);
  const truncated = mockRpc([
    { slot: 10, accounts: [vector.pull] },
    { slot: 11, accounts: [{ ...pool, data: pool.data.slice(0, 510) }, vector.pull] },
  ]);
  await expect(truncated.client.refund(address(vector.pull.address))).rejects.toThrow('InvalidAccount');
});

test('RPC settlement reads coherent state, then only selected items at a newer slot', async () => {
  const { client, requests } = mockRpc([
    { slot: 10, accounts: [vector.pull] }, { slot: 11, accounts: [vector.pool, vector.pull] },
    { slot: 12, accounts: vector.items },
  ]);
  expect(fingerprint(await client.settle(address(vector.pull.address), proof, payer))).toBe(vector.instructions);
  expect(requests.map(r => r.method)).toEqual(Array(3).fill('getMultipleAccounts'));
  expect(requests[1]!.params).toEqual([[vector.pool.address, vector.pull.address], { commitment: 'confirmed', encoding: 'base64', minContextSlot: 10n }]);
  expect(requests[2]!.params[0]).toEqual(vector.items.map(i => i.address));
  expect(requests[2]!.params[1].minContextSlot).toBe(11n);
  const recovery = mockRpc([{ slot: 20, accounts: [vector.partialPull] }]);
  expect(fingerprint(await recovery.client.deliver(address(vector.pull.address), payer))).toBe(vector.recovery);
  expect(recovery.requests).toHaveLength(1);
});

test('RPC rejects missing accounts and regressed context', async () => {
  const missing = mockRpc([{ slot: 10, accounts: [null] }]);
  await expect(missing.client.fetchPull(address(vector.pull.address))).rejects.toThrow('MissingAccount');
  const stale = mockRpc([{ slot: 10, accounts: [vector.pull] }, { slot: 9, accounts: [vector.pool, vector.pull] }]);
  await expect(stale.client.settle(address(vector.pull.address), proof, payer)).rejects.toThrow('InvalidAccount');
  expect(stale.requests).toHaveLength(2);
});
