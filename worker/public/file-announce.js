// Announcing an upload batch to the PC over the sealed control channel (`file-batch`: names, sizes and
// SHA-256 of every file) before any OFFER packet uses the file socket, and waiting for its ack.
// The PC accepts nothing on the file socket that was not announced this way.
// Intent: docs/architecture/file-transfer.md; wire: fixtures/file-transfer-contract.json.
import { Sha256, bytesToHex, fileBatchCancelMessage, fileBatchMessage } from "./file-transfer.js";

const KIB = 1024;
const READ_BLOCK_BYTES = KIB * KIB;
/** How long the PC may take to admit an announced batch. */
const BATCH_ACK_WAIT_MS = 10_000;

/** A viewer that announces right after the epoch switch may beat the PC's rebind: ask once more. */
const NOT_READY_RETRY_MS = 500;
/** Announcements already followed by a `file-batch-cancel` (keyed by the announcing promise). */
const cancelled = new WeakSet();

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

/** Sends the announcement and waits for the PC's ack. */
async function sendBatch(client, first, message) {
  if (!client.sendControl(message)) throw fail("no-peer");
  let timer;
  const timeout = new Promise((_, reject) => { timer = setTimeout(() => reject(fail("timeout")), BATCH_ACK_WAIT_MS); });
  try { return await Promise.race([client.until(first, () => client.batchAcks.get(first.meta.batchId)), timeout]); } finally {
    clearTimeout(timer);
    client.batchAcks.delete(first.meta.batchId);
  }
}

/** Hash and announce `rows` (one batch of `client`), then wait until the PC admits it. */
async function announce(client, rows, terminal, Aborted) {
  const [first] = rows;
  const files = [];
  for (const row of rows) files.push({ name: row.meta.name, size: row.size, mime: row.meta.mime, mtimeMs: row.meta.mtimeMs, sha256: bytesToHex(await hashFile(row, terminal, Aborted)) });
  if (rows.some(row => terminal.has(row.state))) throw new Aborted();
  const message = fileBatchMessage(first.meta.batchId, first.purpose, files, first.meta.drop);
  let ack = await sendBatch(client, first, message);
  if (!ack.ok && ack.error === "not-ready") {
    await new Promise(resolve => setTimeout(resolve, NOT_READY_RETRY_MS));
    if (rows.some(row => terminal.has(row.state))) throw new Aborted();
    ack = await sendBatch(client, first, message);
  }
  if (!ack.ok) throw fail(ack.error || "failed");
}

/**
 * A row of an announced upload batch ended without finishing. Tells the PC (sealed `file-batch-cancel`) to
 * drop the batch, so its slot is free and nothing of it is pasted or dropped later: at once for a paste or
 * drop, which is all-or-nothing, and for a save once none of its rows is still live.
 */
export function settleBatch(client, row, terminal) {
  const batchId = row.meta?.batchId;
  const announced = client.announcing.get(batchId);
  if (row.direction !== "upload" || row.state === "done" || !announced || cancelled.has(announced)) return;
  const live = client.rows.some(other => other.direction === "upload" && other.meta?.batchId === batchId && !terminal.has(other.state));
  if (row.purpose === "save" && live) return;
  cancelled.add(announced);
  client.sendControl(fileBatchCancelMessage(batchId));
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
