import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createDragLock } from "../public/drag-lock.js";

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").split("\r\n").join("\n");

function harness({ lock = () => Promise.resolve() } = {}) {
  const env = { t: 1000, requests: 0, exits: 0, releases: 0, logs: [], shown: [] };
  const dl = createDragLock({
    now: () => env.t,
    requestLock: () => { env.requests++; return lock(); },
    exitLock: () => { env.exits++; },
    releaseButtons: () => { env.releases++; },
    log: detail => env.logs.push(detail),
    onLockedChange: held => env.shown.push(held),
  });
  return { dl, env };
}
const tick = () => new Promise(resolve => setImmediate(resolve));
const press = (dl, extra = {}) => dl.down({ button: 0, pointerMode: "desktop", touchLike: false, ...extra });

test("a monitor switch under a held mouse left button in Work mode requests the lock and logs the grant", async () => {
  const { dl, env } = harness();
  press(dl);
  env.t += 420;
  assert.equal(dl.monitorSwitch({ pointerMode: "desktop" }), true);
  assert.equal(env.requests, 1);
  assert.equal(dl.locked(), false, "not locked until the browser grants it");
  await tick();
  assert.equal(dl.locked(), true);
  assert.deepEqual(env.logs, [{ granted: true, sinceDownMs: 420 }]);
  assert.deepEqual(env.shown, [true]);
});

test("no lock without a held left mouse button in Work mode", () => {
  for (const [name, setup] of [
    ["no press", () => {}],
    ["touch", dl => press(dl, { touchLike: true })],
    ["game mode press", dl => press(dl, { pointerMode: "game" })],
    ["right button", dl => dl.down({ button: 2, pointerMode: "desktop", touchLike: false })],
    ["released", dl => { press(dl); dl.up({ button: 0 }); }],
  ]) {
    const { dl, env } = harness();
    setup(dl);
    assert.equal(dl.monitorSwitch({ pointerMode: "desktop" }), false, name);
    assert.equal(env.requests, 0, name);
  }
  const { dl, env } = harness();
  press(dl);
  assert.equal(dl.monitorSwitch({ pointerMode: "game" }), false, "switched to Game mode");
  assert.equal(env.requests, 0);
});

test("a refused lock (rejected promise) logs once, does nothing further and never retries in that drag", async () => {
  const { dl, env } = harness({ lock: () => Promise.reject(Object.assign(new Error("x"), { name: "NotAllowedError" })) });
  press(dl);
  env.t += 6200;
  dl.monitorSwitch({ pointerMode: "desktop" });
  await tick();
  assert.equal(dl.locked(), false);
  assert.deepEqual(env.logs, [{ granted: false, sinceDownMs: 6200, error: "NotAllowedError" }]);
  assert.equal(dl.monitorSwitch({ pointerMode: "desktop" }), false, "a later switch in the same drag");
  assert.equal(env.requests, 1);
  assert.equal(env.releases, 0, "a refusal touches nothing");
  assert.equal(env.exits, 0);
  // The next drag may try again.
  dl.up({ button: 0 }); press(dl);
  assert.equal(dl.monitorSwitch({ pointerMode: "desktop" }), true);
  assert.equal(env.requests, 2);
});

test("a refusal by pointerlockerror, or a request that throws, is the same refusal", () => {
  const a = harness({ lock: () => new Promise(() => {}) });
  press(a.dl); a.dl.monitorSwitch({ pointerMode: "desktop" });
  a.dl.lockError("pointerlockerror");
  assert.equal(a.dl.locked(), false);
  assert.equal(a.env.logs[0].granted, false);
  assert.equal(a.env.logs[0].error, "pointerlockerror");
  assert.equal(a.dl.monitorSwitch({ pointerMode: "desktop" }), false);

  const b = harness({ lock: () => { throw Object.assign(new Error("x"), { name: "InvalidStateError" }); } });
  press(b.dl);
  b.dl.monitorSwitch({ pointerMode: "desktop" });
  assert.equal(b.env.logs[0].error, "InvalidStateError");
  assert.equal(b.dl.phase, "idle");
});

test("a lock granted by event only (no promise) works", () => {
  const { dl, env } = harness({ lock: () => undefined });
  press(dl);
  dl.monitorSwitch({ pointerMode: "desktop" });
  dl.lockChanged(true);
  assert.equal(dl.locked(), true);
  assert.equal(env.logs.length, 1);
  assert.equal(env.logs[0].granted, true);
});

test("later switches in the same drag keep the lock without a new request or log", async () => {
  const { dl, env } = harness();
  press(dl);
  dl.monitorSwitch({ pointerMode: "desktop" });
  await tick();
  assert.equal(dl.monitorSwitch({ pointerMode: "desktop" }), false);
  assert.equal(env.requests, 1);
  assert.equal(env.logs.length, 1);
  assert.equal(dl.locked(), true);
});

test("mouse-up leaves the lock and does not send a second release", async () => {
  const { dl, env } = harness();
  press(dl); dl.monitorSwitch({ pointerMode: "desktop" }); await tick();
  dl.up({ button: 0 });
  assert.equal(dl.locked(), false);
  assert.equal(env.exits, 1);
  assert.equal(env.releases, 0, "the receiver mouse-up path sends the release");
  assert.deepEqual(env.shown, [true, false]);
  dl.lockChanged(false); // the browser's own event for our exit
  assert.equal(env.releases, 0);
});

test("the lock ending without a mouse-up (Esc, focus, browser) always sends the release", async () => {
  const { dl, env } = harness();
  press(dl); dl.monitorSwitch({ pointerMode: "desktop" }); await tick();
  dl.lockChanged(false);
  assert.equal(env.releases, 1);
  assert.equal(dl.locked(), false);
  assert.deepEqual(env.shown, [true, false]);
  // The button may still be physically down; its later mouse-up must not relock or re-release.
  dl.up({ button: 0 });
  assert.equal(env.releases, 1);
  assert.equal(dl.monitorSwitch({ pointerMode: "desktop" }), false, "a drag whose lock ended does not relock");
});

test("stop() (blur, hidden tab, mode change, disconnect, session end) exits the lock; the receiver sends the release", async () => {
  const { dl, env } = harness();
  press(dl); dl.monitorSwitch({ pointerMode: "desktop" }); await tick();
  dl.stop();
  assert.equal(dl.locked(), false);
  assert.equal(env.exits, 1);
  assert.equal(env.releases, 0, "releaseInput itself sends it");
  dl.stop(); // idempotent
  assert.equal(env.exits, 1);
});

test("a mouse-up while the request is pending drops the lock the moment it is granted", async () => {
  let grant;
  const { dl, env } = harness({ lock: () => new Promise(resolve => { grant = resolve; }) });
  press(dl); dl.monitorSwitch({ pointerMode: "desktop" });
  dl.up({ button: 0 });
  grant(); await tick();
  assert.equal(dl.locked(), false);
  assert.equal(env.exits, 1);
  assert.deepEqual(env.shown, [], "never shown as locked");
});

test("Game mode is untouched: only a left press in Work mode arms anything", () => {
  const { dl, env } = harness();
  dl.down({ button: 0, pointerMode: "game", touchLike: false });
  assert.equal(dl.monitorSwitch({ pointerMode: "game" }), false);
  dl.lockChanged(false); dl.lockChanged(true); dl.stop();
  assert.equal(env.releases + env.exits + env.requests, 0);
});

test("receiver wiring: plain lock, relative motion, positionless buttons, every release path", () => {
  const requestLine = source.split("\n").find(line => line.includes("requestLock: () =>"));
  assert.ok(requestLine, "drag lock created");
  assert.ok(!requestLine.includes("unadjustedMovement"), "speed matches the pointer");
  assert.match(source, /function releaseInput\(send = true\) \{ dragLock\.stop\(\);/);
  assert.match(source, /releaseButtons: \(\) => releaseInput\(\)/);
  assert.match(source, /if \(transitionPlan\.action === "handshake"\) dragLock\.monitorSwitch\(\{ pointerMode \}\);/);
  assert.match(source, /\(pointerMode === "game" && locked\) \|\| dragLock\.locked\(\)/);
  assert.match(source, /pointerMode === "desktop" && !dragLock\.locked\(\)\) setTouchButtonsAtCursor/);
  assert.match(source, /dragLock\.up\(\{ button: e\.button \}\)/);
  assert.match(source, /addEventListener\("pointerlockerror"/);
  // Game mode's own `locked` flag is never set by the temporary lock.
  const handler = source.slice(source.indexOf('document.addEventListener("pointerlockchange"'));
  assert.ok(handler.indexOf("dragLock.lockChanged") < handler.indexOf("locked = document.pointerLockElement"));
});
