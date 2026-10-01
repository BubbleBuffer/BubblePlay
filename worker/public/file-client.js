// File transfer v1, web client side: the `/file-ws` socket, uploads
// (browser -> PC) and offer downloads (PC -> browser). DOM-free apart from
// the optional download sinks at the bottom, so any UI can drive it.
// Intent: .superpawers/file-transfer.md; wire: fixtures/file-transfer-contract.json.
//
// API (FileTransferClient extends EventTarget, fires "change"):
//   new FileTransferClient({ socketUrl })   socketUrl() -> ws(s) URL or null
//   client.transfers                        snapshot rows (see snapshot())
//   client.offer                            { offerId, items, source } | null
//   client.setOffer(fileOfferMessage)       signaling `file-offer` (items [] withdraws)
//   client.upload(files, "save"|"paste")    one batch; returns the row ids
//   client.download(items, sinkFor)         sinkFor(item) -> Promise<sink>; sink = { write(bytes), close(), abort() }
//   client.cancel(id) / client.dismiss(id) / client.clearFinished()
//   client.reset(reason)                    epoch or capability gone: fail everything, drop the offer
import {
  FT, FT_IDLE_CLOSE_MS, FT_MAX_BATCH_FILES, FT_MAX_DATA_BYTES, FT_MAX_FILE_BYTES, FT_MAX_NAME_BYTES,
  FT_ACK_EVERY_BYTES, FT_ACK_EVERY_MS, FT_BLOB_DOWNLOAD_MAX_BYTES,
  FileIncoming, FileSendWindow, Sha256, decodeFilePacket, encodeAccept, encodeAck, encodeCancel, encodeData,
  encodeDone, encodeOffer, encodeRequest, encodeResult, validFileOffer, validOfferMeta,
} from "./file-transfer.js";

const PING_MS = 15_000;
const PEER_WAIT_MS = 10_000;
const READ_BLOCK_BYTES = 1024 * 1024;
/** Keep at most this much queued in the browser's socket buffer. */
const SOCKET_BUFFER_BYTES = 2 * 1024 * 1024;
const TERMINAL = new Set(["done", "failed", "cancelled"]);
const nameEncoder = new TextEncoder();

class TransferAborted extends Error {}

/** Short, user-facing text for protocol error codes. */
export function describeFileError(code) {
  switch (code) {
    case "hash-mismatch": return "The file arrived damaged; nothing was kept.";
    case "size-mismatch": return "The file size changed during the transfer; nothing was kept.";
    case "seq-gap": case "unexpected": return "The transfer got out of step; nothing was kept.";
    case "too-large": return "Larger than 2 GB, the limit per file.";
    case "busy": return "The PC is busy with another file.";
    case "write-failed": return "The PC could not save the file.";
    case "unreadable": return "The PC could not read the file.";
    case "unknown-offer": return "The PC no longer offers this file.";
    case "timeout": return "The transfer stalled.";
    case "no-peer": return "The PC is not ready for files. Try again in a moment.";
    case "disconnected": return "The connection dropped; the transfer was stopped.";
    case "too-many": return "More than 64 files in one go.";
    case "blob-too-large": return "Over 256 MB: this browser can only save it with Chrome or Edge on a computer.";
    case "save-failed": return "The browser could not save the file.";
    default: return code ? `Stopped (${code}).` : "Stopped.";
  }
}

export class FileTransferClient extends EventTarget {
  constructor({ socketUrl, WebSocketImpl = globalThis.WebSocket, now = () => Date.now() } = {}) {
    super();
    this.socketUrl = socketUrl;
    this.WebSocketImpl = WebSocketImpl;
    this.now = now;
    this.rows = [];
    this.nextRowId = 1;
    this.currentOffer = null;
    this.ws = null;
    this.socketState = "closed"; // closed | opening | waiting-peer | ready
    this.nextTransferId = 1;
    this.byTransferId = new Map();
    this.waiters = new Set();
    this.uploadRunning = false;
    this.downloadRunning = false;
    this.idleTimer = null;
    this.pingTimer = null;
    this.ackTimer = null;
    this.peerTimer = null;
  }

  // ---- public ---------------------------------------------------------------

  get transfers() { return this.rows.map(snapshot); }
  get offer() { return this.currentOffer; }

  setOffer(message) {
    if (!validFileOffer(message)) return false;
    this.currentOffer = message.items.length
      ? { offerId: message.offerId, source: message.source, items: message.items.map(item => ({ ...item })) }
      : null;
    this.changed();
    return true;
  }

  upload(files, purpose = "save") {
    const list = Array.from(files || []);
    const ids = [];
    const accepted = [];
    for (const file of list) {
      const row = this.addRow("upload", file?.name || "file", file?.size ?? 0, purpose);
      ids.push(row.id);
      if (accepted.length >= FT_MAX_BATCH_FILES) this.fail(row, "too-many");
      else if (!(file?.size <= FT_MAX_FILE_BYTES)) this.fail(row, "too-large");
      else { row.file = file; accepted.push(row); }
    }
    const batchId = (crypto.getRandomValues(new Uint32Array(1))[0] || 1) >>> 0;
    accepted.forEach((row, index) => {
      row.meta = {
        name: safeName(row.name), size: row.size, mime: (row.file.type || "").slice(0, 255),
        mtimeMs: Number.isSafeInteger(row.file.lastModified) && row.file.lastModified >= 0 ? row.file.lastModified : 0,
        purpose, batchId, batchIndex: index, batchCount: accepted.length,
      };
    });
    this.changed();
    this.pumpUploads();
    return ids;
  }

  download(items, sinkFor) {
    const offer = this.currentOffer;
    if (!offer) return [];
    const ids = [];
    for (const item of items) {
      const row = this.addRow("download", item.name, item.size, "save");
      row.offerId = offer.offerId;
      row.itemId = item.id;
      row.sinkFor = sinkFor;
      ids.push(row.id);
    }
    this.changed();
    this.pumpDownloads();
    return ids;
  }

  cancel(id) {
    const row = this.rows.find(candidate => candidate.id === id);
    if (!row || TERMINAL.has(row.state)) return;
    if (row.transferId !== null && this.ws && this.socketState !== "closed") {
      this.sendBinary(encodeCancel(row.transferId, "cancelled"));
    }
    this.finish(row, "cancelled", null);
  }

  dismiss(id) {
    const row = this.rows.find(candidate => candidate.id === id);
    if (row && TERMINAL.has(row.state)) { this.rows = this.rows.filter(candidate => candidate !== row); this.changed(); }
  }

  clearFinished() {
    this.rows = this.rows.filter(row => !TERMINAL.has(row.state));
    this.changed();
  }

  reset(reason = "disconnected") {
    this.currentOffer = null;
    for (const row of this.rows) if (!TERMINAL.has(row.state)) this.finish(row, "failed", reason);
    this.closeSocket();
    this.changed();
  }

  // ---- rows -------------------------------------------------------------------

  addRow(direction, name, size, purpose) {
    const row = {
      id: this.nextRowId++, direction, name, size, purpose, state: "queued", error: null,
      bytes: 0, startedAt: null, finishedAt: null, transferId: null,
      file: null, meta: null, offerId: null, itemId: null, sinkFor: null, sink: null,
    };
    this.rows.push(row);
    return row;
  }

  fail(row, code) { this.finish(row, "failed", code); }

  finish(row, state, error) {
    if (TERMINAL.has(row.state)) return;
    row.state = state;
    row.error = error;
    row.finishedAt = this.now();
    if (row.transferId !== null) this.byTransferId.delete(row.transferId);
    if (row.sink && state !== "done") { const sink = row.sink; row.sink = null; Promise.resolve().then(() => sink.abort?.()).catch(() => {}); }
    row.file = null;
    this.wake();
    this.changed();
    this.scheduleIdleClose();
  }

  changed() { this.dispatchEvent(new Event("change")); }

  // ---- waiting ----------------------------------------------------------------

  wake() { for (const waiter of [...this.waiters]) waiter(); }

  /** Resolve once `predicate()` is true; re-checked on every socket event and every 15 ms. */
  until(row, predicate) {
    return new Promise((resolve, reject) => {
      let timer = null;
      const check = () => {
        if (row && TERMINAL.has(row.state)) { cleanup(); reject(new TransferAborted()); return; }
        let value;
        try { value = predicate(); } catch (error) { cleanup(); reject(error); return; }
        if (value) { cleanup(); resolve(value); }
      };
      const cleanup = () => { this.waiters.delete(check); clearInterval(timer); };
      this.waiters.add(check);
      timer = setInterval(check, 15);
      check();
    });
  }

  // ---- socket -----------------------------------------------------------------

  async ensureSocket(row) {
    if (this.socketState === "closed") this.openSocket();
    await this.until(row, () => {
      if (this.socketState === "closed") throw Object.assign(new Error("no-peer"), { code: this.socketError || "no-peer" });
      return this.socketState === "ready";
    });
  }

  openSocket() {
    const url = this.socketUrl?.();
    this.socketError = null;
    if (!url || !this.WebSocketImpl) { this.socketState = "closed"; this.socketError = "no-peer"; return; }
    const ws = new this.WebSocketImpl(url);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.socketState = "opening";
    this.nextTransferId = 1;
    ws.onopen = () => { if (this.ws === ws) ws.send(JSON.stringify({ type: "hello", role: "file-client" })); };
    ws.onmessage = event => { if (this.ws === ws) this.onMessage(event.data); };
    ws.onclose = () => { if (this.ws === ws) this.onSocketClosed(); };
    ws.onerror = () => {};
    clearInterval(this.pingTimer);
    this.pingTimer = setInterval(() => { if (this.ws === ws && ws.readyState === 1) ws.send(JSON.stringify({ type: "ping" })); }, PING_MS);
    clearTimeout(this.peerTimer);
    this.peerTimer = setTimeout(() => {
      if (this.ws === ws && this.socketState !== "ready") { this.socketError = "no-peer"; this.closeSocket(); this.wake(); }
    }, PEER_WAIT_MS);
  }

  closeSocket() {
    const ws = this.ws;
    this.ws = null;
    this.socketState = "closed";
    clearInterval(this.pingTimer);
    clearInterval(this.ackTimer);
    clearTimeout(this.peerTimer);
    clearTimeout(this.idleTimer);
    this.ackTimer = null;
    if (ws) { try { ws.close(1000, "idle"); } catch {} }
  }

  onSocketClosed() {
    const wasReady = this.socketState === "ready";
    this.closeSocket();
    for (const row of this.rows) {
      if (TERMINAL.has(row.state)) continue;
      // A drop cancels the active transfers (no resume in v1); queued ones wait for a new socket.
      if (row.transferId !== null || !wasReady) this.fail(row, wasReady ? "disconnected" : (this.socketError || "no-peer"));
    }
    this.wake();
    this.pumpUploads();
    this.pumpDownloads();
  }

  scheduleIdleClose() {
    clearTimeout(this.idleTimer);
    if (this.rows.some(row => !TERMINAL.has(row.state)) || !this.ws) return;
    this.idleTimer = setTimeout(() => {
      if (!this.rows.some(row => !TERMINAL.has(row.state))) this.closeSocket();
    }, FT_IDLE_CLOSE_MS);
  }

  sendBinary(bytes) {
    if (!this.ws || this.ws.readyState !== 1) return false;
    this.ws.send(bytes);
    return true;
  }

  onMessage(data) {
    if (typeof data === "string") {
      let message;
      try { message = JSON.parse(data); } catch { return; }
      if (message.type === "file-peer") {
        if (message.present === true) { this.socketState = "ready"; clearTimeout(this.peerTimer); }
        else if (this.socketState !== "ready") this.socketState = "waiting-peer";
        this.wake();
      }
      return;
    }
    const packet = decodeFilePacket(data);
    if (!packet.ok) return;
    const row = this.byTransferId.get(packet.transferId);
    if (!row) return;
    if (row.direction === "upload") this.onUploadPacket(row, packet);
    else this.onDownloadPacket(row, packet);
    this.wake();
  }

  takeTransferId() {
    const id = this.nextTransferId;
    this.nextTransferId = (this.nextTransferId + 1) >>> 0 || 1;
    return id;
  }

  // ---- uploads ----------------------------------------------------------------

  async pumpUploads() {
    if (this.uploadRunning) return;
    this.uploadRunning = true;
    try {
      for (;;) {
        const row = this.rows.find(candidate => candidate.direction === "upload" && candidate.state === "queued");
        if (!row) break;
        await this.runUpload(row).catch(error => {
          if (!(error instanceof TransferAborted)) this.fail(row, error?.code || "disconnected");
        });
      }
    } finally {
      this.uploadRunning = false;
      this.scheduleIdleClose();
    }
  }

  async runUpload(row) {
    row.state = "connecting";
    this.changed();
    await this.ensureSocket(row);
    row.transferId = this.takeTransferId();
    this.byTransferId.set(row.transferId, row);
    row.window = new FileSendWindow();
    row.accepted = false;
    row.result = null;
    this.sendBinary(encodeOffer(row.transferId, row.meta));
    await this.until(row, () => row.accepted);
    row.state = "active";
    row.startedAt = this.now();
    this.changed();
    const hash = new Sha256();
    let seq = 0;
    let lastChange = 0;
    for (let offset = 0; offset < row.size; offset += READ_BLOCK_BYTES) {
      const block = new Uint8Array(await row.file.slice(offset, Math.min(row.size, offset + READ_BLOCK_BYTES)).arrayBuffer());
      if (TERMINAL.has(row.state)) throw new TransferAborted();
      for (let at = 0; at < block.byteLength; at += FT_MAX_DATA_BYTES) {
        const chunk = block.subarray(at, Math.min(block.byteLength, at + FT_MAX_DATA_BYTES));
        await this.until(row, () => row.window.canSend(chunk.byteLength) &&
          (this.ws?.bufferedAmount ?? 0) < SOCKET_BUFFER_BYTES);
        hash.update(chunk);
        if (!this.sendBinary(encodeData(row.transferId, seq, chunk))) throw Object.assign(new Error(), { code: "disconnected" });
        seq += 1;
        row.window.onSent(chunk.byteLength);
      }
      const now = this.now();
      if (now - lastChange > 100) { lastChange = now; this.changed(); }
    }
    if (row.window.sent !== row.size) throw Object.assign(new Error(), { code: "size-mismatch" });
    this.sendBinary(encodeDone(row.transferId, hash.finish()));
    row.state = "verifying";
    this.changed();
    const result = await this.until(row, () => row.result);
    if (result.ok) { row.bytes = row.size; this.finish(row, "done", null); }
    else this.fail(row, result.error || "failed");
  }

  onUploadPacket(row, packet) {
    switch (packet.type) {
      case FT.ACCEPT: row.accepted = true; break;
      case FT.ACK:
        if (!row.window?.onAck(packet.total)) { this.sendBinary(encodeCancel(row.transferId, "unexpected")); this.fail(row, "unexpected"); break; }
        row.bytes = packet.total;
        break;
      case FT.RESULT: row.result = { ok: packet.json.ok, error: packet.json.error ?? null }; break;
      case FT.CANCEL: this.fail(row, packet.json.reason || "cancelled"); break;
      default: break;
    }
  }

  // ---- downloads --------------------------------------------------------------

  async pumpDownloads() {
    if (this.downloadRunning) return;
    this.downloadRunning = true;
    try {
      for (;;) {
        const row = this.rows.find(candidate => candidate.direction === "download" && candidate.state === "queued");
        if (!row) break;
        await this.runDownload(row).catch(error => {
          if (!(error instanceof TransferAborted)) this.fail(row, error?.code || "disconnected");
        });
      }
    } finally {
      this.downloadRunning = false;
      this.scheduleIdleClose();
    }
  }

  async runDownload(row) {
    row.state = "connecting";
    this.changed();
    try {
      row.sink = await row.sinkFor({ id: row.itemId, name: row.name, size: row.size });
    } catch (error) {
      throw Object.assign(new Error(), { code: error?.code || "save-failed" });
    }
    if (TERMINAL.has(row.state)) throw new TransferAborted();
    await this.ensureSocket(row);
    row.transferId = this.takeTransferId();
    this.byTransferId.set(row.transferId, row);
    row.incoming = null;
    row.hash = new Sha256();
    row.writes = Promise.resolve();
    row.written = 0;
    row.acked = 0;
    row.lastAckAt = this.now();
    row.doneHash = null;
    row.writeError = null;
    this.sendBinary(encodeRequest(row.transferId, row.offerId, row.itemId));
    await this.until(row, () => row.incoming);
    row.state = "active";
    row.startedAt = this.now();
    this.startAckTimer();
    this.changed();
    await this.until(row, () => row.doneHash || row.writeError);
    await row.writes;
    if (row.writeError) throw Object.assign(new Error(), { code: "save-failed" });
    row.state = "verifying";
    this.changed();
    const verdict = row.incoming.onDone(Array.from(row.doneHash), Array.from(row.hash.finish()));
    if (verdict) {
      this.sendBinary(encodeResult(row.transferId, false, verdict));
      this.fail(row, verdict);
      return;
    }
    const sink = row.sink;
    try { await sink.close(); } catch {
      this.sendBinary(encodeResult(row.transferId, false, "save-failed"));
      this.fail(row, "save-failed");
      return;
    }
    row.sink = null;
    this.sendBinary(encodeResult(row.transferId, true));
    row.bytes = row.size;
    this.finish(row, "done", null);
  }

  onDownloadPacket(row, packet) {
    switch (packet.type) {
      case FT.OFFER: {
        const meta = packet.json;
        if (row.incoming || !validOfferMeta(meta) || meta.size !== row.size) {
          this.sendBinary(encodeCancel(row.transferId, "unexpected"));
          this.fail(row, "unexpected");
          return;
        }
        row.incoming = new FileIncoming(row.transferId, meta);
        this.sendBinary(encodeAccept(row.transferId));
        break;
      }
      case FT.DATA: {
        if (!row.incoming || row.doneHash) return;
        const error = row.incoming.onData(packet.seq, packet.payload.byteLength);
        if (error) { this.sendBinary(encodeCancel(row.transferId, error)); this.fail(row, error); return; }
        const bytes = packet.payload.slice();
        row.hash.update(bytes);
        const sink = row.sink;
        row.writes = row.writes.then(async () => {
          if (row.writeError || TERMINAL.has(row.state)) return;
          try { await sink.write(bytes); } catch (writeError) { row.writeError = writeError; this.wake(); return; }
          row.written += bytes.byteLength;
          row.bytes = row.written;
          this.maybeAck(row);
        });
        break;
      }
      case FT.DONE: if (row.incoming) row.doneHash = packet.sha256; break;
      case FT.CANCEL: this.fail(row, packet.json.reason || "cancelled"); break;
      default: break;
    }
  }

  /** ACK what is written (not just received), so a slow disk slows the PC. */
  maybeAck(row, force = false) {
    if (TERMINAL.has(row.state) || row.written === row.acked) return;
    const now = this.now();
    if (force || row.written - row.acked >= FT_ACK_EVERY_BYTES || now - row.lastAckAt >= FT_ACK_EVERY_MS || row.written === row.size) {
      row.acked = row.written;
      row.lastAckAt = now;
      this.sendBinary(encodeAck(row.transferId, row.written));
      if (now - (row.lastChangeAt || 0) > 100) { row.lastChangeAt = now; this.changed(); }
    }
  }

  startAckTimer() {
    if (this.ackTimer) return;
    this.ackTimer = setInterval(() => {
      const active = this.rows.filter(row => row.direction === "download" && row.state === "active");
      if (!active.length) { clearInterval(this.ackTimer); this.ackTimer = null; return; }
      for (const row of active) this.maybeAck(row);
    }, 50);
  }
}

function snapshot(row) {
  return {
    id: row.id, direction: row.direction, name: row.name, size: row.size, purpose: row.purpose,
    state: row.state, error: row.error, bytes: row.bytes, startedAt: row.startedAt, finishedAt: row.finishedAt,
  };
}

/** Names go to the PC as-is (the PC sanitises them); only enforce the byte limit. */
function safeName(name) {
  let out = String(name || "file") || "file";
  while (nameEncoder.encode(out).byteLength > FT_MAX_NAME_BYTES) out = out.slice(0, -1);
  return out || "file";
}

// ---- download sinks (browser only) ---------------------------------------------

/** True when the File System Access API can save straight to disk. */
export function canSaveToDisk() {
  return typeof globalThis.showSaveFilePicker === "function" && typeof globalThis.showDirectoryPicker === "function";
}

/**
 * Choose where downloads go, inside the click handler (pickers need a user
 * gesture). Returns `sinkFor(item)` for FileTransferClient.download, or null
 * when the user dismissed the picker.
 */
export async function chooseDownloadTarget(items) {
  if (canSaveToDisk()) {
    try {
      if (items.length === 1) {
        const handle = await globalThis.showSaveFilePicker({ suggestedName: items[0].name });
        return async () => fsaSink(handle);
      }
      const directory = await globalThis.showDirectoryPicker({ mode: "readwrite", id: "bubbleplay-downloads" });
      return async item => fsaSink(await directory.getFileHandle(await freeName(directory, item.name), { create: true }));
    } catch (error) {
      if (error?.name === "AbortError") return null;
      throw error;
    }
  }
  return async item => {
    if (item.size > FT_BLOB_DOWNLOAD_MAX_BYTES) throw Object.assign(new Error(), { code: "blob-too-large" });
    return blobSink(item.name);
  };
}

async function fsaSink(handle) {
  const writable = await handle.createWritable();
  return {
    write: bytes => writable.write(bytes),
    close: () => writable.close(),
    // Discards the temporary file; nothing partial is left behind.
    abort: () => writable.abort(),
  };
}

async function freeName(directory, name) {
  const dot = name.lastIndexOf(".");
  const stem = dot > 0 ? name.slice(0, dot) : name;
  const extension = dot > 0 ? name.slice(dot) : "";
  for (let n = 0; n < 1000; n += 1) {
    const candidate = n === 0 ? name : `${stem} (${n})${extension}`;
    try { await directory.getFileHandle(candidate); } catch { return candidate; }
  }
  return name;
}

function blobSink(name) {
  let parts = [];
  return {
    write: async bytes => { parts.push(bytes); },
    close: async () => {
      const url = URL.createObjectURL(new Blob(parts));
      parts = [];
      const link = document.createElement("a");
      link.href = url;
      link.download = name;
      link.rel = "noopener";
      document.body.append(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
    },
    abort: async () => { parts = []; },
  };
}
