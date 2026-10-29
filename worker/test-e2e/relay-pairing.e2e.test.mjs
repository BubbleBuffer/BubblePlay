// Pairing from a /pair link on the native relay: a code with no window, a wrong code and then the right one, the PC's
// pair-denied, a window that is used up by its first match, and the per-prefix failure limit.
import { randomBytes } from "node:crypto";
import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import {
  CASE_TIMEOUT_MS, closeSocket, connectViewer, loadIdentity, openHost, pairing, sendThenSync, startRelay,
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

// A 32-byte value that is not the admit hash of any window.
const wrongPair = () => randomBytes(32).toString("base64url");

// The relay refuses a pairing viewer with 1008 and an empty reason, and never sends it hello-ok.
async function assertRefused(connection, what) {
  const close = await connection.box.done;
  assert.equal(close.code, 1008, what);
  assert.equal(close.reason, "", what);
  assert.equal(connection.box.items.some(message => message.type === "hello-ok"), false, what);
}

test("a pairing code with no open window is refused 1008 with an empty reason", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, []);
  const viewer = await connectViewer(relay, VIEWER, PC.id, { pair: wrongPair() });
  try {
    await assertRefused(viewer, "a code with no window");
  } finally {
    await closeSocket(viewer.socket);
    await closeSocket(host.socket);
  }
});

test("pair-open admits the right code, a wrong code is refused, and the PC is told of the pairing viewer", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, []);
  await sendThenSync(host, { type: "pair-open", admit: pairing.admit });
  const sockets = [];
  try {
    const wrong = await connectViewer(relay, VIEWER, PC.id, { pair: wrongPair() });
    sockets.push(wrong.socket);
    await assertRefused(wrong, "a wrong code while the window is open");

    const right = await connectViewer(relay, VIEWER, PC.id, { pair: pairing.admit });
    sockets.push(right.socket);
    const hello = await right.box.take(message => message.type === "hello-ok", "pairing hello-ok");
    assert.equal(hello.pc, PC.id);
    assert.equal(hello.pairing, true);
    assert.equal(hello.ticket, undefined);
    assert.match(hello.mediaEpoch, /^[0-9a-f]{32}$/);

    const joined = await host.box.take(message => message.type === "peer-joined", "peer-joined");
    assert.deepEqual(joined, { type: "peer-joined", mediaEpoch: hello.mediaEpoch, pairing: true });

    host.socket.send(JSON.stringify({ type: "pair-denied" }));
    assert.deepEqual(await right.box.take(message => message.type === "pair-denied", "pair-denied"), { type: "pair-denied" });

    // The window is used up by its first match, so the same code is refused the second time.
    const again = await connectViewer(relay, VIEWER, PC.id, { pair: pairing.admit });
    sockets.push(again.socket);
    await assertRefused(again, "the same code after a match");
  } finally {
    for (const socket of sockets) await closeSocket(socket);
    await closeSocket(host.socket);
  }
});

test("five failed codes from one prefix refuse even the right code", { timeout: CASE_TIMEOUT_MS }, async () => {
  // A relay of its own, so the failures counted here are not shared with the tests above.
  const own = await startRelay();
  try {
    const host = await openHost(own, PC, []);
    await sendThenSync(host, { type: "pair-open", admit: pairing.admit });
    try {
      for (let attempt = 1; attempt <= 5; attempt += 1) {
        const wrong = await connectViewer(own, VIEWER, PC.id, { pair: wrongPair() });
        await assertRefused(wrong, `wrong code ${attempt}`);
        await closeSocket(wrong.socket);
      }
      // The sixth attempt carries the right code, but the prefix is over its limit, so nothing is compared.
      const right = await connectViewer(own, VIEWER, PC.id, { pair: pairing.admit });
      await assertRefused(right, "the right code after the prefix limit");
      await closeSocket(right.socket);
    } finally {
      await closeSocket(host.socket);
    }
  } finally {
    await own.stop();
  }
});
