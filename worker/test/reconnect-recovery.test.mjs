import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { createViewerSession } from "../public/dk-session.js";
import {
  RECOVERY_BUDGET_MS, recycledAuthTimeoutMs, shouldRetryAuthTimeout,
} from "../public/recovery.js";

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").split(String.fromCharCode(13, 10)).join("\n");
const fn = name => {
  const start = source.indexOf(`function ${name}(`);
  assert.ok(start >= 0, name);
  return source.slice(start, source.indexOf("\n}\n", start) + 3);
};

test("a recycled session waits 3 s, backing off to a cap of 8 s", () => {
  assert.deepEqual([0, 1, 2, 3, 9].map(recycledAuthTimeoutMs), [3000, 4500, 6750, 8000, 8000]);
  assert.equal(recycledAuthTimeoutMs(-1), 3000);
});

test("only a silent PC of an established session is retried, and only inside the budget", () => {
  const base = { reason: "auth-timeout", established: true, budgetStartMs: 1000, nowMs: 2000 };
  assert.equal(shouldRetryAuthTimeout(base), true);
  assert.equal(shouldRetryAuthTimeout({ ...base, established: false }), false, "first connection keeps today's terminal timeout");
  assert.equal(shouldRetryAuthTimeout({ ...base, nowMs: 1000 + RECOVERY_BUDGET_MS }), false);
  assert.equal(shouldRetryAuthTimeout({ ...base, budgetStartMs: null }), false);
  for (const reason of ["pc-key-changed", "handshake-failed", "bind-failed", "missing-cap:control-aead-v1", "dtls-bind-required", "cap-missing", "control-failed"]) {
    assert.equal(shouldRetryAuthTimeout({ ...base, reason }), false, reason);
  }
});

test("the session honours the shorter auth-host wait", async () => {
  const timers = [];
  const session = createViewerSession({
    identity: { id: "x", publicRaw: new Uint8Array(32), privateKey: null }, pcId: "pc",
    pinnedPublic: new Uint8Array(32), relayOrigin: "https://relay.test", epoch: "epoch", send: () => true,
    schedule: (fn, delay) => { timers.push(delay); return timers.length; }, cancel() {}, authTimeoutMs: 3000,
  });
  await session.start();
  assert.equal(timers[0], 3000);
});

function receiverHarness() {
  const output = { ended: [], recycled: [], logs: [] };
  let now = 100_000;
  const context = vm.createContext({
    everAuthenticated: false, recoveryLossMs: null, recoveryBudgetStartMs: null, recoveryAttempts: 0,
    performance: { now: () => now }, Math, recycledAuthTimeoutMs, shouldRetryAuthTimeout,
    viewerLog: (level, event, detail) => output.logs.push({ level, event, detail }),
    recycleSignaling: reason => output.recycled.push(reason),
    endAuthenticatedSession: refusal => output.ended.push(refusal),
  });
  vm.runInContext(["noteConnectionLoss", "noteSessionAuthenticated", "sessionAuthTimeoutMs", "noteRecovered", "handleSessionFailure"].map(fn).join("\n"), context);
  return { context, output, advance: ms => { now += ms; } };
}

test("auth timeout on a first connection ends the session as before", () => {
  const h = receiverHarness();
  assert.equal(h.context.sessionAuthTimeoutMs(), undefined);
  h.context.handleSessionFailure({ title: "PC not reachable" }, "auth-timeout");
  assert.equal(h.output.ended.length, 1);
  assert.deepEqual(h.output.recycled, []);
});

test("auth timeout after a loss retries with a shorter, growing wait until the budget is spent", () => {
  const h = receiverHarness();
  h.context.noteSessionAuthenticated();
  h.context.noteConnectionLoss(); // peer-left
  h.context.recoveryAttempts = 1; // first reconnect socket opened
  assert.equal(h.context.sessionAuthTimeoutMs(), 3000);
  h.advance(3500);
  h.context.handleSessionFailure({ title: "PC not reachable" }, "auth-timeout");
  assert.deepEqual(h.output.recycled, ["auth-timeout"]);
  assert.equal(h.output.ended.length, 0);
  h.context.recoveryAttempts = 2;
  assert.equal(h.context.sessionAuthTimeoutMs(), 4500);
  h.advance(RECOVERY_BUDGET_MS);
  h.context.handleSessionFailure({ title: "PC not reachable" }, "auth-timeout");
  assert.equal(h.output.ended.length, 1, "the budget is spent: the existing terminal screen");
  assert.equal(h.output.recycled.length, 1);
});

test("a real refusal after a loss is never retried", () => {
  const h = receiverHarness();
  h.context.noteSessionAuthenticated();
  h.context.noteConnectionLoss();
  h.context.handleSessionFailure({ title: "PC key changed" }, "pc-key-changed");
  assert.equal(h.output.ended.length, 1);
  assert.deepEqual(h.output.recycled, []);
});

test("one info line reports the total time and attempts when the first frame returns", () => {
  const h = receiverHarness();
  h.context.noteRecovered();
  assert.deepEqual(h.output.logs, [], "nothing on a first connection");
  h.context.noteSessionAuthenticated();
  h.context.noteConnectionLoss();
  h.context.recoveryAttempts = 2;
  h.advance(3120);
  h.context.noteRecovered();
  h.context.noteRecovered();
  assert.equal(h.output.logs.length, 1);
  assert.equal(h.output.logs[0].level, "info");
  assert.deepEqual({ ...h.output.logs[0].detail }, { recoveryMs: 3120, attempts: 2 });
  assert.equal(h.context.recoveryLossMs, null);
});
