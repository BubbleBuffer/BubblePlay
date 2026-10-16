import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import worker, { Room } from "../src/index.js";
import { RELAY_SECRETS } from "./helpers/relay-secrets.mjs";
import {
  FT,
  FT_ACK_EVERY_BYTES,
  FT_ACK_EVERY_MS,
  FT_HEADER_BYTES,
  FT_IDLE_CLOSE_MS,
  FT_MAX_BATCH_FILES,
  FT_MAX_CONTROL_PAYLOAD_BYTES,
  FT_MAX_DATA_BYTES,
  FT_MAX_FILE_BYTES,
  FT_MAX_MESSAGE_BYTES,
  FT_MAX_NAME_BYTES,
  FT_TYPE_NAMES,
  FT_WINDOW_BYTES,
  FileIncoming,
  FileSendWindow,
  decodeFilePacket,
  encodeAccept,
  encodeAck,
  encodeCancel,
  encodeData,
  encodeDone,
  encodeOffer,
  encodeRequest,
  encodeResult,
  relayAcceptsFilePacket,
  validFileOffer,
} from "../public/file-transfer.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/file-transfer-contract.json", import.meta.url), "utf8"));
const hex = value => Uint8Array.from(value.match(/../g) || [], byte => parseInt(byte, 16));
const toHex = bytes => Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");

function rejectedBytes(entry) {
  if (entry.hex !== undefined) return hex(entry.hex);
  const bytes = new Uint8Array(entry.totalBytes).fill(0xab);
  bytes.set(hex(entry.headerHex));
  return bytes;
}

function reencode(packet) {
  switch (packet.type) {
    case FT.OFFER: return encodeOffer(packet.transferId, packet.json);
    case FT.ACCEPT: return encodeAccept(packet.transferId);
    case FT.DATA: return encodeData(packet.transferId, packet.seq, packet.payload);
    case FT.ACK: return encodeAck(packet.transferId, packet.total);
    case FT.DONE: return encodeDone(packet.transferId, packet.sha256);
    case FT.CANCEL: return encodeCancel(packet.transferId, packet.json.reason);
    case FT.RESULT: return encodeResult(packet.transferId, packet.json.ok, packet.json.error);
    case FT.REQUEST: return encodeRequest(packet.transferId, packet.json.offerId, packet.json.id);
    default: throw new Error(`unknown type ${packet.type}`);
  }
}

test("file transfer limits and header layout match the shared fixture", () => {
  assert.equal(contract.magic, "BPF1");
  assert.equal(contract.headerBytes, FT_HEADER_BYTES);
  const limits = contract.limits;
  assert.equal(limits.maxDataBytes, FT_MAX_DATA_BYTES);
  assert.equal(limits.maxControlPayloadBytes, FT_MAX_CONTROL_PAYLOAD_BYTES);
  assert.equal(limits.maxMessageBytes, FT_MAX_MESSAGE_BYTES);
  assert.equal(limits.windowBytes, FT_WINDOW_BYTES);
  assert.equal(limits.ackEveryBytes, FT_ACK_EVERY_BYTES);
  assert.equal(limits.ackEveryMs, FT_ACK_EVERY_MS);
  assert.equal(limits.maxFileBytes, FT_MAX_FILE_BYTES);
  assert.equal(limits.maxBatchFiles, FT_MAX_BATCH_FILES);
  assert.equal(limits.maxNameBytes, FT_MAX_NAME_BYTES);
  assert.equal(limits.idleCloseMs, FT_IDLE_CLOSE_MS);
  for (const [name, value] of Object.entries(contract.types)) assert.equal(FT_TYPE_NAMES[value], name);
  assert.deepEqual(
    { type: contract.header.type, transferId: contract.header.transferId, seq: contract.header.seq },
    { type: 4, transferId: 8, seq: 12 });
});

test("file transfer packets decode and re-encode byte-exact (fixture from the Rust encoder)", () => {
  for (const entry of contract.packets) {
    const bytes = hex(entry.hex);
    const packet = decodeFilePacket(bytes);
    assert.equal(packet.ok, true, entry.name);
    assert.equal(packet.typeName, entry.type, entry.name);
    assert.equal(packet.transferId, entry.transferId, entry.name);
    assert.equal(packet.seq, entry.seq, entry.name);
    assert.equal(toHex(packet.payload), entry.payloadHex, entry.name);
    if (packet.json) assert.deepEqual(packet.json, entry.decoded, entry.name);
    if (packet.type === FT.ACK) assert.equal(packet.total, entry.decoded.total);
    if (packet.type === FT.DONE) assert.equal(toHex(packet.sha256), entry.decoded.sha256);
    if (packet.type === FT.DATA) assert.equal(toHex(packet.payload), entry.decoded.dataHex);
    assert.equal(toHex(reencode(packet)), entry.hex, `${entry.name} re-encodes`);
    assert.equal(relayAcceptsFilePacket(bytes.buffer, "file-client"), true, entry.name);
  }
});

test("file transfer rejects malformed packets with the fixture's reason, and the relay drops them", () => {
  for (const entry of contract.rejected) {
    const bytes = rejectedBytes(entry);
    const packet = decodeFilePacket(bytes);
    assert.equal(packet.ok, false, entry.name);
    assert.equal(packet.reason, entry.reason, entry.name);
    assert.equal(relayAcceptsFilePacket(bytes, "file-client"), false, entry.name);
    assert.equal(relayAcceptsFilePacket(bytes, "file-host"), false, entry.name);
  }
  // REQUEST only travels client -> host; text frames never.
  const request = contract.packets.find(entry => entry.type === "REQUEST");
  assert.equal(relayAcceptsFilePacket(hex(request.hex), "file-host"), false);
  assert.equal(relayAcceptsFilePacket("{}", "file-client"), false);
});

test("receiver sessions reach the fixture outcome (seq gap, size and hash mismatch)", () => {
  for (const session of contract.receiverSessions) {
    let incoming = null;
    let hash = createHash("sha256");
    let outcome = "ok";
    for (const step of session.packets) {
      const packet = decodeFilePacket(hex(step));
      assert.equal(packet.ok, true, session.name);
      let error = null;
      if (packet.type === FT.OFFER) incoming = new FileIncoming(packet.transferId, packet.json);
      else if (packet.type === FT.DATA) {
        error = incoming.onData(packet.seq, packet.payload.byteLength);
        if (!error) hash.update(packet.payload);
      } else if (packet.type === FT.DONE) {
        error = incoming.onDone(packet.sha256, new Uint8Array(hash.digest()));
        hash = createHash("sha256");
      }
      if (error) { outcome = error; break; }
    }
    assert.equal(outcome, session.outcome, session.name);
  }
});

test("file-offer signaling messages follow the fixture", () => {
  for (const entry of contract.signaling.fileOffer) {
    let parsed = null;
    try { parsed = JSON.parse(entry.text); } catch { /* invalid */ }
    assert.equal(validFileOffer(parsed), entry.accepted, entry.text.slice(0, 120));
  }
  assert.equal(JSON.parse(contract.signaling.hostCapabilities.text).fileTransfer, 1);
});

test("send window and ACK cadence", () => {
  const window = new FileSendWindow();
  let chunks = 0;
  while (window.canSend(FT_MAX_DATA_BYTES)) { window.onSent(FT_MAX_DATA_BYTES); chunks += 1; }
  assert.equal(chunks * FT_MAX_DATA_BYTES, FT_WINDOW_BYTES);
  assert.equal(window.onAck(FT_WINDOW_BYTES + 1), false);
  assert.equal(window.onAck(FT_ACK_EVERY_BYTES), true);
  assert.equal(window.onAck(FT_ACK_EVERY_BYTES - 1), false);
  assert.equal(window.canSend(FT_ACK_EVERY_BYTES), true);
  assert.equal(window.canSend(FT_ACK_EVERY_BYTES + 1), false);

  const incoming = new FileIncoming(1, { size: 600 * 1024 });
  const acks = [];
  let now = 1_000;
  for (let seq = 0; seq < 5; seq += 1) {
    assert.equal(incoming.onData(seq, FT_MAX_DATA_BYTES), null);
    const total = incoming.ackDue(now);
    if (total !== null) acks.push(total);
    now += 1;
  }
  assert.deepEqual(acks, [65_536, 5 * 65_536]);
  assert.equal(incoming.onData(5, FT_MAX_DATA_BYTES), null);
  assert.equal(incoming.ackDue(now), null);
  assert.equal(incoming.ackDue(now + FT_ACK_EVERY_MS), 6 * 65_536);
});

class FakeSocket {
  constructor(role, extra = {}) {
    this.attachment = { role, ...extra };
    this.readyState = 1;
    this.messages = [];
    this.closed = [];
  }
  deserializeAttachment() { return this.attachment; }
  serializeAttachment(value) { this.attachment = value; }
  send(value) { this.messages.push(value); }
  close(code, reason) { this.closed.push({ code, reason }); this.readyState = 3; }
}

test("file routes are gated like the media routes and paired per epoch in the MediaRoom class", async () => {
  const selected = [];
  const fileResponse = new Response("file", { status: 200 });
  let gateStatus = 204;
  const env = {
    ...RELAY_SECRETS,
    APPROVAL_METHODS: "host",
    GATE: { idFromName: () => "gate", get: () => ({ fetch: async (request) => (new URL(request.url).pathname === "/__pc/resolve"
      ? Response.json({ pc: null }) : new Response(null, { status: gateStatus })) }) },
    ROOM: { idFromName: name => name, get: () => ({ fetch: async () => new Response("signal") }) },
    MEDIA_ROOM: { idFromName: name => { selected.push(name); return name; }, get: () => ({ fetch: async () => fileResponse }) },
  };
  const epoch = "0123456789abcdef0123456789abcdef";
  const hostHeaders = { Upgrade: "websocket", Authorization: `Bearer ${RELAY_SECRETS.HOST_GATE_TOKEN}` };
  assert.equal(await worker.fetch(new Request(`https://x.example/host-file-ws?epoch=${epoch}`, { headers: hostHeaders }), env), fileResponse);
  assert.equal(await worker.fetch(new Request(`https://x.example/file-ws?epoch=${epoch}`, { headers: { Upgrade: "websocket" } }), env), fileResponse);
  assert.deepEqual(selected, [`file-v1:${epoch}`, `file-v1:${epoch}`]);
  // No host token, no cookie, or no epoch: 404 before any room is touched.
  assert.equal((await worker.fetch(new Request(`https://x.example/host-file-ws?epoch=${epoch}`,
    { headers: { Upgrade: "websocket", Authorization: "Bearer wrong" } }), env)).status, 404);
  assert.equal((await worker.fetch(new Request("https://x.example/file-ws?epoch=BAD", { headers: { Upgrade: "websocket" } }), env)).status, 404);
  gateStatus = 401;
  assert.equal((await worker.fetch(new Request(`https://x.example/file-ws?epoch=${epoch}`, { headers: { Upgrade: "websocket" } }), env)).status, 404);
  assert.equal(selected.length, 2);
});

test("file relay forwards valid packets both ways, answers pings and drops pre-hello traffic", async () => {
  const host = new FakeSocket("file-host");
  const client = new FakeSocket("file-client");
  const room = new Room({ getWebSockets: () => [host, client] }, {});
  const offer = hex(contract.packets.find(entry => entry.name === "offer").hex).buffer;
  const request = hex(contract.packets.find(entry => entry.name === "request").hex).buffer;
  const ack = hex(contract.packets.find(entry => entry.name === "ack").hex).buffer;
  await room.webSocketMessage(client, offer);
  await room.webSocketMessage(client, request);
  await room.webSocketMessage(host, ack);
  assert.deepEqual(host.messages, [offer, request]);
  assert.deepEqual(client.messages, [ack]);
  await room.webSocketMessage(client, '{"type":"ping"}');
  assert.equal(client.messages.at(-1), '{"type":"pong"}');
  await room.webSocketMessage(client, '{"type":"file-offer"}');
  assert.equal(host.messages.length, 2, "text is never relayed on the file socket");

  const fresh = new FakeSocket(null, { expectedRole: "file-client" });
  const lone = new Room({ getWebSockets: () => [fresh] }, {});
  await lone.webSocketMessage(fresh, offer);
  assert.deepEqual(fresh.messages, []);
});

test("file relay closes both sides on a malformed packet or a REQUEST from the host", async () => {
  for (const [sender, packet] of [
    ["client", rejectedBytes(contract.rejected.find(entry => entry.name === "nonzero-flags"))],
    ["client", rejectedBytes(contract.rejected.find(entry => entry.name === "oversize-data"))],
    ["host", hex(contract.packets.find(entry => entry.name === "request").hex)],
  ]) {
    const host = new FakeSocket("file-host");
    const client = new FakeSocket("file-client");
    const room = new Room({ getWebSockets: () => [host, client].filter(socket => socket.readyState === 1) }, {});
    const from = sender === "host" ? host : client;
    const to = sender === "host" ? client : host;
    await room.webSocketMessage(from, packet.buffer);
    assert.equal(to.messages.filter(message => typeof message !== "string").length, 0);
    assert.equal(from.closed[0]?.code, 1008);
    assert.equal(to.closed[0]?.code, 1001);
  }
});

test("file hello announces presence and a leaving side closes the other", async () => {
  const host = new FakeSocket(null, { expectedRole: "file-host" });
  let sockets = [host];
  const room = new Room({ getWebSockets: () => sockets }, {});
  await room.webSocketMessage(host, JSON.stringify({ type: "hello", role: "file-host" }));
  assert.deepEqual(host.messages.map(message => JSON.parse(message)), [
    { type: "hello-ok" }, { type: "file-peer", present: false }]);
  const client = new FakeSocket(null, { expectedRole: "file-client" });
  sockets = [host, client];
  await room.webSocketMessage(client, JSON.stringify({ type: "hello", role: "file-client" }));
  assert.deepEqual(JSON.parse(host.messages.at(-1)), { type: "file-peer", present: true });
  assert.deepEqual(JSON.parse(client.messages.at(-1)), { type: "file-peer", present: true });
  // A client socket cannot claim the host role.
  const impostor = new FakeSocket(null, { expectedRole: "file-client" });
  await room.webSocketMessage(impostor, JSON.stringify({ type: "hello", role: "file-host" }));
  assert.equal(impostor.closed[0]?.code, 1008);

  client.readyState = 3;
  sockets = [host];
  await room.webSocketClose(client, 1000, "", true);
  assert.deepEqual(host.closed, [{ code: 1001, reason: "file-peer-left" }]);
});

test("signaling relays only valid host file-offers and fileTransfer capabilities", async () => {
  const host = new FakeSocket("host");
  const client = new FakeSocket("client");
  const room = new Room({ getWebSockets: () => [host, client] }, {});
  for (const entry of contract.signaling.fileOffer) {
    const before = client.messages.length;
    await room.webSocketMessage(host, entry.text);
    assert.equal(client.messages.length - before, entry.accepted ? 1 : 0, entry.text.slice(0, 100));
  }
  // Clients cannot send offers.
  const valid = contract.signaling.fileOffer.find(entry => entry.accepted).text;
  await room.webSocketMessage(client, valid);
  assert.equal(host.messages.length, 0);
  await room.webSocketMessage(host, contract.signaling.hostCapabilities.text);
  assert.equal(client.messages.at(-1), contract.signaling.hostCapabilities.text);
  const before = client.messages.length;
  await room.webSocketMessage(host, contract.signaling.hostCapabilities.text.replace('"fileTransfer":1', '"fileTransfer":"yes"'));
  assert.equal(client.messages.length, before);
});
