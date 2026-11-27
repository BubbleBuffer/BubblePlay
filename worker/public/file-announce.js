// Announcing an upload batch to the PC over the sealed control channel (`file-batch`: names, sizes and
// SHA-256 of every file) before any OFFER packet uses the file socket, and waiting for its ack.
// The PC accepts nothing on the file socket that was not announced this way.
// Intent: docs/architecture/file-transfer.md; wire: fixtures/file-transfer-contract.json.
import { Sha256, bytesToHex, fileBatchMessage } from "./file-transfer.js";

const KIB = 1024;
const READ_BLOCK_BYTES = KIB * KIB;
/** How long the PC may take to admit an announced batch. */
const BATCH_ACK_WAIT_MS = 10_000;

const fail = code => Object.assign(new Error(code), { code });

/** SHA-256 of `row.file`, read in blocks; fails with `size-mismatch` when the bytes read differ from `row.size`. */
async function hashFile(row, terminal, Aborted) {
  const hash = new Sha256();
  let read = 0;
  for (let offset = 0; offset < row.size; offset += READ_BLOCK_BYTES) {
    const block = new Uint8Array(await row.file.slice(offset, Math.min(row.size, offset + READ_BLOCK_BYTES)).arrayBuffer());
    if (terminal.has(row.state)) throw new Aborted();
    hash.update(block);
    read += block.byteLength;
  }
  if (read !== row.size) throw fail("size-mismatch");
  return hash.finish();
}

/** Hash and announce `rows` (one batch of `client`), then wait until the PC admits it. */
async function announce(client, rows, terminal, Aborted) {
  const [first] = rows;
  const files = [];
  for (const row of rows) files.push({ name: row.meta.name, size: row.size, sha256: bytesToHex(await hashFile(row, terminal, Aborted)) });
  if (rows.some(row => terminal.has(row.state))) throw new Aborted();
  if (!client.sendControl(fileBatchMessage(first.meta.batchId, first.purpose, files, first.meta.drop))) throw fail("no-peer");
  let timer;
  const timeout = new Promise((_, reject) => { timer = setTimeout(() => reject(fail("timeout")), BATCH_ACK_WAIT_MS); });
  const ack = await Promise.race([client.until(first, () => client.batchAcks.get(first.meta.batchId)), timeout]).finally(() => clearTimeout(timer));
  client.batchAcks.delete(first.meta.batchId);
  if (!ack.ok) throw fail(ack.error || "failed");
}

/**
 * Announce the batch of `row` (all its live rows, indexed in order) once and wait until the PC admits it.
 * A batch that cannot be announced fails as a whole. `terminal` is the set of finished row states and
 * `Aborted` the error that ends a cancelled transfer quietly.
 */
export async function ensureAnnounced(client, row, terminal, Aborted) {
  const batchId = row.meta.batchId;
  if (!client.announcing.has(batchId)) {
    const rows = client.rows.filter(other => other.direction === "upload" && other.meta?.batchId === batchId && !terminal.has(other.state));
    rows.forEach((member, index) => { member.meta.batchIndex = index; member.meta.batchCount = rows.length; });
    client.announcing.set(batchId, announce(client, rows, terminal, Aborted).catch(error => {
      for (const member of rows) if (!terminal.has(member.state)) client.fail(member, error instanceof Aborted ? "cancelled" : (error?.code || "disconnected"));
      throw error;
    }));
  }
  try { await client.announcing.get(batchId); } catch { throw new Aborted(); }
}
