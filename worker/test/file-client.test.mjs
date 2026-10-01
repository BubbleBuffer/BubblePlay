import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import {
  FT, Sha256, decodeFilePacket, encodeAccept, encodeAck, encodeCancel, encodeData, encodeDone, encodeOffer, encodeResult,
} from "../public/file-transfer.js";
import { FileTransferClient } from "../public/file-client.js";

test("incremental SHA-256 matches node:crypto across block boundaries", () => {
  for (const size of [0, 1, 55, 56, 63, 64, 65, 119, 120, 1000, 200_003]) {
    const bytes = randomBytes(size);
    const hash = new Sha256();
    for (let offset = 0; offset < size;) {
      const take = Math.min(size - offset, 1 + ((offset * 7919) % 7000));
      hash.update(bytes.subarray(offset, offset + take));
      offset += take;
    }
    assert.equal(Buffer.from(hash.finish()).toString("hex"), createHash("sha256").update(bytes).digest("hex"), `size ${size}`);
  }
});

/** A fake relay + PC: answers the client like the relay and host do. */
class FakeHost {
  constructor({ files = {}, corrupt = false, peerPresent = true } = {}) {
    this.files = files;
    this.corrupt = corrupt;
    this.peerPresent = peerPresent;
    this.uploads = new Map();
    this.saved = [];
    this.sockets = [];
    this.sentByClient = [];
    const host = this;
    this.WebSocket = class {
      constructor(url) {
        this.url = url;
        this.readyState = 0;
        this.bufferedAmount = 0;
        host.sockets.push(this);
        setTimeout(() => { this.readyState = 1; this.onopen?.(); }, 0);
      }
      send(data) { host.receive(this, data); }
      close() { if (this.readyState === 3) return; this.readyState = 3; setTimeout(() => this.onclose?.({}), 0); }
    };
  }

  deliver(ws, data) { setTimeout(() => { if (ws.readyState === 1) ws.onmessage?.({ data: data instanceof Uint8Array ? data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) : data }); }, 0); }

  receive(ws, data) {
    if (typeof data === "string") {
      const message = JSON.parse(data);
      if (message.type === "hello") {
        this.deliver(ws, JSON.stringify({ type: "hello-ok" }));
        this.deliver(ws, JSON.stringify({ type: "file-peer", present: this.peerPresent }));
      }
      return;
    }
    const packet = decodeFilePacket(data);
    assert.ok(packet.ok, `client sent an invalid packet: ${packet.reason}`);
    this.sentByClient.push(packet);
    const id = packet.transferId;
    switch (packet.type) {
      case FT.OFFER:
        this.uploads.set(id, { meta: packet.json, chunks: [], hash: createHash("sha256"), received: 0 });
        this.deliver(ws, encodeAccept(id));
        break;
      case FT.DATA: {
        const upload = this.uploads.get(id);
        upload.chunks.push(Buffer.from(packet.payload));
        upload.hash.update(packet.payload);
        upload.received += packet.payload.byteLength;
        this.deliver(ws, encodeAck(id, upload.received));
        break;
      }
      case FT.DONE: {
        const upload = this.uploads.get(id);
        const ok = Buffer.from(packet.sha256).equals(upload.hash.digest()) && upload.received === upload.meta.size;
        if (ok) this.saved.push({ meta: upload.meta, bytes: Buffer.concat(upload.chunks) });
        this.deliver(ws, encodeResult(id, ok, ok ? undefined : "hash-mismatch"));
        break;
      }
      case FT.REQUEST: {
        const file = this.files[packet.json.id];
        if (!file) { this.deliver(ws, encodeCancel(id, "unknown-offer")); break; }
        this.deliver(ws, encodeOffer(id, { name: file.name, size: file.bytes.length, mime: "", mtimeMs: 0, purpose: "save", batchId: 1, batchIndex: 0, batchCount: 1 }));
        this.pending = { id, file };
        break;
      }
      case FT.ACCEPT: {
        const { file } = this.pending;
        let seq = 0;
        for (let offset = 0; offset < file.bytes.length; offset += 65536) {
          const chunk = Buffer.from(file.bytes.subarray(offset, offset + 65536));
          if (this.corrupt && offset === 0) chunk[0] ^= 0xff;
          this.deliver(ws, encodeData(id, seq++, chunk));
        }
        this.deliver(ws, encodeDone(id, createHash("sha256").update(file.bytes).digest()));
        break;
      }
      default: break;
    }
  }
}

function settled(client, ids) {
  return new Promise(resolve => {
    const check = () => {
      const rows = client.transfers.filter(row => ids.includes(row.id));
      if (rows.every(row => ["done", "failed", "cancelled"].includes(row.state))) { client.removeEventListener("change", check); resolve(rows); }
    };
    client.addEventListener("change", check);
    check();
  });
}

function memorySink() {
  const sink = { parts: [], closed: false, aborted: false };
  sink.write = async bytes => { sink.parts.push(Buffer.from(bytes)); };
  sink.close = async () => { sink.closed = true; };
  sink.abort = async () => { sink.aborted = true; };
  return sink;
}

test("uploads a batch through the window and the PC verifies each hash", async () => {
  const host = new FakeHost();
  const client = new FileTransferClient({ socketUrl: () => "wss://example.test/file-ws?epoch=x", WebSocketImpl: host.WebSocket });
  const big = randomBytes(3 * 1024 * 1024 + 17);
  const ids = client.upload([new File([big], "big.bin"), new File([new Uint8Array([7])], "one.txt", { type: "text/plain" })], "paste");
  const rows = await settled(client, ids);
  assert.deepEqual(rows.map(row => row.state), ["done", "done"]);
  assert.equal(host.saved.length, 2);
  assert.ok(host.saved[0].bytes.equals(big));
  assert.deepEqual([...host.saved[1].bytes], [7]);
  assert.equal(host.saved[0].meta.purpose, "paste");
  assert.equal(host.saved[0].meta.batchCount, 2);
  assert.equal(host.saved[1].meta.batchIndex, 1);
  assert.equal(host.saved[0].meta.batchId, host.saved[1].meta.batchId);
  const transferIds = host.sentByClient.filter(packet => packet.type === FT.OFFER).map(packet => packet.transferId);
  assert.equal(new Set(transferIds).size, 2, "transfer ids are unique per socket");
  client.reset();
});

test("files over 2 GiB are refused before anything is sent", async () => {
  const host = new FakeHost();
  const client = new FileTransferClient({ socketUrl: () => "wss://x", WebSocketImpl: host.WebSocket });
  const [id] = client.upload([{ name: "huge.iso", size: 2 * 1024 * 1024 * 1024 + 1, type: "", lastModified: 0, slice() { throw new Error("read"); } }]);
  const [row] = await settled(client, [id]);
  assert.equal(row.state, "failed");
  assert.equal(row.error, "too-large");
  assert.equal(host.sockets.length, 0);
});

test("downloads an offered file, verifies it and closes the sink", async () => {
  const bytes = randomBytes(2 * 1024 * 1024 + 3);
  const host = new FakeHost({ files: { 4: { name: "report.pdf", bytes } } });
  const client = new FileTransferClient({ socketUrl: () => "wss://x", WebSocketImpl: host.WebSocket });
  assert.ok(client.setOffer({ type: "file-offer", protocol: 1, offerId: 9, items: [{ id: 4, name: "report.pdf", size: bytes.length }], source: "sendto" }));
  const sink = memorySink();
  const ids = client.download(client.offer.items, async () => sink);
  const [row] = await settled(client, ids);
  assert.equal(row.state, "done", row.error);
  assert.ok(sink.closed && !sink.aborted);
  assert.ok(Buffer.concat(sink.parts).equals(bytes));
  const request = host.sentByClient.find(packet => packet.type === FT.REQUEST);
  assert.deepEqual(request.json, { offerId: 9, id: 4 });
  const result = host.sentByClient.find(packet => packet.type === FT.RESULT);
  assert.deepEqual(result.json, { ok: true });
  assert.ok(host.sentByClient.some(packet => packet.type === FT.ACK && packet.total === bytes.length));
  client.reset();
});

test("a damaged download is rejected and its partial output discarded", async () => {
  const bytes = randomBytes(200_000);
  const host = new FakeHost({ files: { 1: { name: "a.bin", bytes } }, corrupt: true });
  const client = new FileTransferClient({ socketUrl: () => "wss://x", WebSocketImpl: host.WebSocket });
  client.setOffer({ type: "file-offer", protocol: 1, offerId: 1, items: [{ id: 1, name: "a.bin", size: bytes.length }], source: "clipboard" });
  const sink = memorySink();
  const [row] = await settled(client, client.download(client.offer.items, async () => sink));
  assert.equal(row.state, "failed");
  assert.equal(row.error, "hash-mismatch");
  assert.ok(sink.aborted && !sink.closed);
  assert.deepEqual(host.sentByClient.find(packet => packet.type === FT.RESULT).json, { ok: false, error: "hash-mismatch" });
  client.reset();
});

test("a withdrawn file is reported, and a user cancel sends CANCEL", async () => {
  const host = new FakeHost();
  const client = new FileTransferClient({ socketUrl: () => "wss://x", WebSocketImpl: host.WebSocket });
  client.setOffer({ type: "file-offer", protocol: 1, offerId: 2, items: [{ id: 5, name: "gone.txt", size: 3 }], source: "clipboard" });
  const sink = memorySink();
  const [gone] = await settled(client, client.download(client.offer.items, async () => sink));
  assert.equal(gone.error, "unknown-offer");
  assert.ok(sink.aborted);

  const [id] = client.upload([new File([randomBytes(5 * 1024 * 1024)], "slow.bin")]);
  await new Promise(resolve => setTimeout(resolve, 5));
  client.cancel(id);
  const [row] = await settled(client, [id]);
  assert.equal(row.state, "cancelled");
  assert.ok(host.sentByClient.some(packet => packet.type === FT.CANCEL && packet.transferId !== 0));
  client.reset();
});

test("offers replace each other and an empty offer withdraws", () => {
  const client = new FileTransferClient({ socketUrl: () => null });
  client.setOffer({ type: "file-offer", protocol: 1, offerId: 1, items: [{ id: 1, name: "a", size: 1 }], source: "clipboard" });
  client.setOffer({ type: "file-offer", protocol: 1, offerId: 2, items: [{ id: 1, name: "b", size: 2 }, { id: 2, name: "c", size: 3 }], source: "sendto" });
  assert.equal(client.offer.offerId, 2);
  assert.equal(client.offer.items.length, 2);
  assert.equal(client.setOffer({ type: "file-offer", protocol: 1, offerId: 3, items: [{ id: 1, name: "", size: 1 }], source: "sendto" }), false);
  assert.equal(client.offer.offerId, 2, "invalid offers are ignored");
  client.setOffer({ type: "file-offer", protocol: 1, offerId: 4, items: [], source: "clipboard" });
  assert.equal(client.offer, null);
});

test("no PC on the file socket fails the transfer with a clear reason", async () => {
  const host = new FakeHost({ peerPresent: false });
  const client = new FileTransferClient({ socketUrl: () => "wss://x", WebSocketImpl: host.WebSocket });
  const [id] = client.upload([new File([new Uint8Array([1])], "a")]);
  // The socket ends while still waiting for the PC (as after the 10 s peer wait).
  setTimeout(() => host.sockets[0].close(), 30);
  const [row] = await settled(client, [id]);
  assert.equal(row.state, "failed");
  assert.equal(row.error, "no-peer");
});
