// Routing and admission on the native relay: refusals before an upgrade, a host with a wrong token, a second admission
// that replaces the first viewer, and an admitted viewer of a PC that is offline.
import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import WebSocket from "ws";
import {
  CASE_TIMEOUT_MS, admitViewer, attach, closeSocket, connectViewer, delay, loadIdentity, openHost, opened, startRelay, upgradeStatus,
} from "./harness.mjs";

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

test("unknown routes and missing query parameters are refused 404 before any upgrade", { timeout: CASE_TIMEOUT_MS }, async () => {
  const zeros = "0".repeat(32);
  for (const path of [
    `/nope-ws?pc=${PC.id}`,
    "/ws",
    "/host-ws",
    `/media-ws?pc=${PC.id}`,
    `/host-file-ws?pc=${PC.id}&epoch=${zeros}`,
    `/file-ws?pc=${PC.id}&epoch=BAD`,
  ]) {
    assert.equal(await upgradeStatus(`${relay.ws}${path}`), 404, path);
  }
});

test("a host with a wrong token is closed 1008 with the reason 'invalid hello'", { timeout: CASE_TIMEOUT_MS }, async () => {
  const socket = new WebSocket(`${relay.ws}/host-ws?pc=${PC.id}`);
  const box = attach(socket, "pc");
  await opened(socket);
  try {
    socket.send(JSON.stringify({ type: "hello", role: "host", token: "not-the-token", id: PC.id, public: PC.publicB64 }));
    const close = await box.done;
    assert.equal(close.code, 1008);
    assert.equal(close.reason, "invalid hello");
    assert.equal(box.items.some(message => message.type === "register-challenge"), false);
  } finally {
    await closeSocket(socket);
  }
});

test("a second admission replaces the first viewer, and the first epoch's data sockets are refused", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, [VIEWER.id]);
  const first = await admitViewer(relay, VIEWER, PC.id);
  const second = await admitViewer(relay, VIEWER, PC.id);
  try {
    assert.notEqual(second.hello.mediaEpoch, first.hello.mediaEpoch);
    const replaced = await first.box.done;
    assert.equal(replaced.code, 1008);
    assert.equal(replaced.reason, "peer replaced");

    assert.equal(await upgradeStatus(`${relay.ws}/media-ws?pc=${PC.id}&epoch=${first.hello.mediaEpoch}`), 404);

    second.socket.send(JSON.stringify({ type: "ping" }));
    assert.deepEqual(await second.box.take(message => message.type === "pong", "pong"), { type: "pong" });
  } finally {
    await closeSocket(first.socket);
    await closeSocket(second.socket);
    await closeSocket(host.socket);
  }
});

test("an admitted viewer of an offline PC gets hello-ok with no peer event and stays connected", { timeout: CASE_TIMEOUT_MS }, async t => {
  const host = await openHost(relay, PC, [VIEWER.id]);
  await closeSocket(host.socket);

  // The admission lives on the PC record, not on the host socket, and nothing checks that the host is online.
  const viewer = await connectViewer(relay, VIEWER, PC.id);
  try {
    const hello = await viewer.box.take(message => message.type === "hello-ok", "hello-ok with the PC offline");
    assert.equal(hello.pc, PC.id);
    assert.equal(typeof hello.ticket, "string");
    assert.match(hello.mediaEpoch, /^[0-9a-f]{32}$/);

    await delay(300);
    assert.equal(viewer.box.ended, false, "the viewer socket is still open with the PC offline");
    assert.equal(viewer.box.items.some(message => message.type === "peer-joined" || message.type === "media-peer"), false);
    t.diagnostic("offline PC: hello-ok with ticket and mediaEpoch, no peer event, socket open after 300 ms");
  } finally {
    await closeSocket(viewer.socket);
  }
});
