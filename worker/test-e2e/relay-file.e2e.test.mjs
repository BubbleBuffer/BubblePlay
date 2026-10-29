// File-transfer sockets on the native relay: the BPF1 packets from fixtures/file-transfer-contract.json are relayed
// unchanged in both directions, presence is reported, and a close on one side closes the other.
import { readFileSync } from "node:fs";
import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import {
  CASE_TIMEOUT_MS, admitViewer, closeSocket, loadIdentity, openDataSocket, openHost, startRelay, upgradeStatus,
} from "./harness.mjs";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/file-transfer-contract.json", import.meta.url), "utf8"));
const packet = name => Buffer.from(contract.packets.find(entry => entry.name === name).hex, "hex");

let relay = null;
let PC = null;
let VIEWER = null;

before(async () => {
  PC = await loadIdentity("pc");
  VIEWER = await loadIdentity("viewer");
  relay = await startRelay();
});

after(async () => {
  await relay?.stop();
  relay = null;
});

// Sends one packet from one side and waits for the other side to get exactly those bytes.
async function relayed(from, to, bytes, what) {
  from.socket.send(bytes);
  const item = await to.box.take(message => message.binary && message.binary.equals(bytes), what);
  return item.binary;
}

test("file sockets relay the BPF1 upload both ways, report presence, and close the other side when one leaves", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, [VIEWER.id]);
  const viewer = await admitViewer(relay, VIEWER, PC.id);
  const epoch = viewer.hello.mediaEpoch;
  const fileHost = await openDataSocket(relay, "/host-file-ws", PC.id, epoch, { type: "hello", role: "file-host", ticket: host.ticket });
  const fileViewer = await openDataSocket(relay, "/file-ws", PC.id, epoch, { type: "hello", role: "file-viewer", ticket: viewer.hello.ticket });
  try {
    assert.deepEqual(await fileHost.box.take(message => message.type === "hello-ok", "file host hello-ok"), { type: "hello-ok" });
    assert.deepEqual(await fileHost.box.take(message => message.type === "file-peer", "file host file-peer"), { type: "file-peer", present: false });
    assert.deepEqual(await fileViewer.box.take(message => message.type === "hello-ok", "file viewer hello-ok"), { type: "hello-ok" });
    assert.deepEqual(await fileViewer.box.take(message => message.type === "file-peer", "file viewer file-peer"), { type: "file-peer", present: true });
    assert.deepEqual(await fileHost.box.take(message => message.type === "file-peer", "file host file-peer present"), { type: "file-peer", present: true });

    assert.deepEqual(await relayed(fileViewer, fileHost, packet("offer"), "OFFER from the viewer"), packet("offer"));
    assert.deepEqual(await relayed(fileHost, fileViewer, packet("accept"), "ACCEPT from the PC"), packet("accept"));
    assert.deepEqual(await relayed(fileViewer, fileHost, packet("data"), "DATA from the viewer"), packet("data"));
    assert.deepEqual(await relayed(fileHost, fileViewer, packet("ack"), "ACK from the PC"), packet("ack"));
    assert.deepEqual(await relayed(fileViewer, fileHost, packet("done"), "DONE from the viewer"), packet("done"));
    assert.deepEqual(await relayed(fileHost, fileViewer, packet("result-ok"), "RESULT from the PC"), packet("result-ok"));
    assert.deepEqual(await relayed(fileViewer, fileHost, packet("request"), "REQUEST from the viewer"), packet("request"));

    fileViewer.socket.send(JSON.stringify({ type: "ping" }));
    assert.deepEqual(await fileViewer.box.take(message => message.type === "pong", "pong"), { type: "pong" });

    await closeSocket(fileHost.socket);
    const left = await fileViewer.box.done;
    assert.equal(left.code, 1001);
    assert.equal(left.reason, "file-peer-left");
  } finally {
    await closeSocket(fileHost.socket);
    await closeSocket(fileViewer.socket);
    await closeSocket(viewer.socket);
    await closeSocket(host.socket);
  }
});

test("file sockets refuse a viewer ticket on the host side, a forged viewer ticket, and a stale epoch", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, [VIEWER.id]);
  const viewer = await admitViewer(relay, VIEWER, PC.id);
  const epoch = viewer.hello.mediaEpoch;
  const sockets = [];
  try {
    // The host side issues its own tickets, so the viewer's ticket is not valid there.
    const wrongRole = await openDataSocket(relay, "/host-file-ws", PC.id, epoch, { type: "hello", role: "file-host", ticket: viewer.hello.ticket });
    sockets.push(wrongRole);
    const wrongRoleClose = await wrongRole.box.done;
    assert.equal(wrongRoleClose.code, 1008);
    assert.equal(wrongRoleClose.reason, "");

    const forged = await openDataSocket(relay, "/file-ws", PC.id, epoch, { type: "hello", role: "file-viewer", ticket: "A".repeat(43) });
    sockets.push(forged);
    const forgedClose = await forged.box.done;
    assert.equal(forgedClose.code, 1008);
    assert.equal(forgedClose.reason, "");

    assert.equal(await upgradeStatus(`${relay.ws}/file-ws?pc=${PC.id}&epoch=${"0".repeat(32)}`), 404);
  } finally {
    for (const socket of sockets) await closeSocket(socket.socket);
    await closeSocket(viewer.socket);
    await closeSocket(host.socket);
  }
});
