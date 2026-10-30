import test from "node:test";
import assert from "node:assert/strict";
import { createApp, normalizePairingInput, readPairingLink, sortPcs, FORGET_ENDPOINT } from "../public/dk-app.js";

const VIEWER_ID = "47Z3-3QX1-AJH6-2RKB";
const PC_ID = "77VH-7M56-8GJK-Y12J";
const PLAIN = JSON.stringify({ id: VIEWER_ID, label: "Test", kdf: null, aead: null });
const PROTECTED = JSON.stringify({ id: VIEWER_ID, label: "Test", kdf: { alg: "argon2id" }, aead: { alg: "aes-256-gcm" } });
const PUBLIC = "A".repeat(43);

function fakeGate() {
  const calls = [];
  const gate = { calls, last: name => calls.filter(call => call[0] === name).at(-1)?.[1] };
  for (const name of ["showKeyScreen", "showPcList", "showPairing", "showRefusal", "showBusy", "hide"])
    gate[name] = arg => calls.push([name, arg]);
  return gate;
}

function memoryStore({ key = null, pcs = [], backup: initialBackup = null } = {}) {
  const records = new Map(pcs.map(pc => [pc.id, { ...pc }]));
  const log = [];
  let backup = initialBackup;
  return {
    log, records,
    get backup() { return backup; },
    async load() { return key; },
    async save(text) { key = text; },
    async loadBackup() { return backup; },
    async saveBackup(text) { backup = text; },
    async clearBackup() { backup = null; },
    async clear() { log.push(["clear"]); key = null; backup = null; },
    async listPcs() { return [...records.values()]; },
    async getPc(id) { return records.get(id) ?? null; },
    async putPc(record) { records.set(record.id, record); },
    async renamePc(id, name) { log.push(["rename", id, name]); records.get(id).name = name; },
    async removePc(id) { log.push(["remove", id]); records.delete(id); },
    async touchPc(id) { log.push(["touch", id]); },
  };
}

const IDENTITY_KEY = { id: VIEWER_ID, publicKey: new Uint8Array(32), privateKey: { type: "private" } };
function fakeUnlock(calls = []) {
  return async (text, passphrase) => {
    calls.push(passphrase);
    if (JSON.parse(text).kdf && passphrase !== "right") {
      const error = new Error("authentication failed");
      error.code = passphrase === undefined ? "passphrase-required" : "authentication-failed";
      throw error;
    }
    return IDENTITY_KEY;
  };
}

function fakeLocation(href) {
  const location = { href, assigned: [], reloads: 0, assign(url) { this.assigned.push(url); }, reload() { this.reloads++; } };
  const history = { calls: [], replaceState(_state, _title, url) { this.calls.push(url); location.href = new URL(url, location.href).href; } };
  return { location, history };
}

// Stands in for the real conversion: same content, shared format name, as devicekey.js writes it.
const SHARED_NAME = "bubblebuffer-device-key";
const fakeConvert = async (text, passphrase) => JSON.stringify({ ...JSON.parse(text), format: SHARED_NAME });

function app({ key = PLAIN, pcs = [], href = "https://relay.test/", keyUi = null, fetch, openPairing, unlockCalls, store, convert = fakeConvert, unlock } = {}) {
  const gate = fakeGate();
  store ??= memoryStore({ key, pcs });
  const { location, history } = fakeLocation(href);
  const sessions = [];
  const pairings = [];
  const instance = createApp({
    gate, store, keyUi, location, history, unlock: unlock ?? fakeUnlock(unlockCalls), convert,
    loadSession: async () => ({ startSession: options => sessions.push(options), endSession: () => sessions.push("ended") }),
    fetch: fetch ?? (async () => ({ ok: true })),
    reload: () => location.reload(),
    openPairing: openPairing ?? (options => { pairings.push(options); return { close() {} }; }),
  });
  return { instance, gate, store, location, history, sessions, pairings };
}

const flush = () => new Promise(resolve => setImmediate(resolve));

test("no stored key shows the setup screen, whose create/import open the existing dialogs", async () => {
  let created = 0;
  const h = app({ key: null, keyUi: { createKey: async () => { created++; return false; }, importKey: async () => false } });
  await h.instance.start();
  const screen = h.gate.last("showKeyScreen");
  assert.equal(screen.mode, "setup");
  assert.equal(typeof screen.onCreate, "function");
  assert.equal(typeof screen.onImport, "function");
  assert.equal(screen.onRemove, undefined);
  assert.equal(h.gate.last("showPcList"), undefined);
  await screen.onCreate();
  assert.equal(created, 1);
  // Cancelled: still no key, so setup again.
  assert.equal(h.gate.calls.filter(call => call[0] === "showKeyScreen").length, 2);
});

test("creating a key in setup continues to the PC list", async () => {
  const h = app({ key: null });
  const store = h.store;
  const h2 = createApp({ gate: h.gate, store, location: h.location, history: h.history, unlock: fakeUnlock(),
    keyUi: { createKey: async () => { await store.save(PLAIN); return true; }, importKey: async () => false },
    loadSession: async () => ({}) });
  await h2.start();
  await h.gate.last("showKeyScreen").onCreate();
  assert.equal(h.gate.last("showPcList").deviceId, VIEWER_ID);
});

test("protected key: wrong passphrase rejects with wrong-passphrase, then the right one unlocks", async () => {
  const unlockCalls = [];
  const h = app({ key: PROTECTED, unlockCalls });
  await h.instance.start();
  const screen = h.gate.last("showKeyScreen");
  assert.equal(screen.mode, "unlock");
  assert.equal(typeof screen.onRemove, "function");
  await assert.rejects(screen.onUnlock("wrong"), error => error.code === "wrong-passphrase");
  assert.equal(h.gate.last("showPcList"), undefined);
  assert.equal(h.instance.identity(), null);
  await screen.onUnlock("right");
  assert.deepEqual(unlockCalls, ["wrong", "right"]);
  assert.equal(h.gate.last("showPcList").deviceId, VIEWER_ID);
  assert.equal(h.instance.identity().privateKey, IDENTITY_KEY.privateKey);
});

test("plain key unlocks without asking and shows the sorted PC list", async () => {
  const pcs = [
    { id: "AAAA-AAAA-AAAA-AAAA", name: "Old", public: PUBLIC, paired: 1, lastUsed: 10 },
    { id: PC_ID, name: "Desk", public: PUBLIC, paired: 1, lastUsed: 20 },
    { id: "BBBB-BBBB-BBBB-BBBB", name: "Never", public: PUBLIC, paired: 1, lastUsed: null },
  ];
  const h = app({ pcs });
  await h.instance.start();
  assert.equal(h.gate.last("showKeyScreen"), undefined);
  const list = h.gate.last("showPcList");
  assert.equal(list.deviceId, VIEWER_ID);
  assert.deepEqual(list.pcs, [
    { id: PC_ID, name: "Desk", lastUsed: 20 },
    { id: "AAAA-AAAA-AAAA-AAAA", name: "Old", lastUsed: 10 },
    { id: "BBBB-BBBB-BBBB-BBBB", name: "Never", lastUsed: null },
  ]);
  assert.deepEqual(sortPcs(pcs).map(pc => pc.name), ["Desk", "Old", "Never"]);
  for (const name of ["onConnect", "onRename", "onRemove", "onPair", "onForgetBrowser", "onLock"]) assert.equal(typeof list[name], "function");
});

test("?pc= naming a stored PC connects directly with the unlocked identity", async () => {
  const h = app({ pcs: [{ id: PC_ID, name: "Desk", public: PUBLIC, paired: 1, lastUsed: null }], href: `https://relay.test/?pc=${PC_ID}&transport=wss` });
  await h.instance.start();
  assert.equal(h.gate.last("showPcList"), undefined);
  assert.equal(h.sessions.length, 1);
  assert.equal(h.sessions[0].pc, PC_ID);
  assert.equal(h.sessions[0].pinnedPublic, PUBLIC);
  assert.equal(h.sessions[0].gate, h.gate);
  assert.deepEqual(h.sessions[0].identity, { id: VIEWER_ID, publicRaw: IDENTITY_KEY.publicKey, privateKey: IDENTITY_KEY.privateKey });
  assert.match(h.location.href, /transport=wss/);
});

test("?pc= for an unknown PC shows the list instead", async () => {
  const h = app({ href: `https://relay.test/?pc=${PC_ID}` });
  await h.instance.start();
  assert.equal(h.sessions.length, 0);
  assert.ok(h.gate.last("showPcList"));
});

test("/pair fragment is dropped at once and pairing gets normalised values", async () => {
  const h = app({ href: "https://relay.test/pair#pc=77vh-7m56-8gjk-y12j&code=7k3qd-m9xlp" });
  await h.instance.start();
  assert.deepEqual(h.history.calls, ["/pair"]);
  assert.equal(new URL(h.location.href).hash, "");
  assert.equal(h.pairings.length, 1);
  assert.equal(h.pairings[0].pc, PC_ID);
  assert.equal(h.pairings[0].href, "https://relay.test/pair");
  assert.match(h.pairings[0].pair, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(h.pairings[0].origin, "https://relay.test");
  assert.equal(h.gate.last("showPairing").state, "connecting");
  assert.equal(h.gate.last("showPairing").pcId, PC_ID);
});

test("the fragment is dropped before the key screen even when unlock is pending", async () => {
  const h = app({ key: PROTECTED, href: "https://relay.test/pair?x=1#pc=77VH-7M56-8GJK-Y12J&code=7K3QDM9X2P" });
  await h.instance.start();
  assert.deepEqual(h.history.calls, ["/pair?x=1"]);
  assert.equal(h.pairings.length, 0);
  await h.gate.last("showKeyScreen").onUnlock("right");
  assert.equal(h.pairings[0].pc, PC_ID);
});

test("an invalid pairing link shows a failed pairing screen with a way back", async () => {
  const h = app({ href: "https://relay.test/pair#pc=nope&code=7K3QDM9X2P" });
  await h.instance.start();
  const screen = h.gate.last("showPairing");
  assert.equal(screen.state, "failed");
  assert.match(screen.message, /PC ID/);
  await screen.onBack();
  assert.ok(h.gate.last("showPcList"));
  assert.equal(h.pairings.length, 0);
});

test("gate onPair normalises and rejects invalid input with a message", async () => {
  const h = app();
  await h.instance.start();
  const list = h.gate.last("showPcList");
  await assert.rejects(list.onPair({ pc: PC_ID, code: "short" }), /pairing code/);
  await list.onPair({ pc: ` ${PC_ID.toLowerCase()} `, code: "7k3qd m9x2p" });
  assert.equal(h.pairings[0].pc, PC_ID);
  assert.deepEqual(normalizePairingInput({ pc: "77vh7m568gjky12j", code: "oiL0O-12345" }), { pc: PC_ID, code: "0110012345" });
  assert.equal(readPairingLink("https://relay.test/?pc=x"), null);
});

test("pairing refusal shows the failed screen; cancel returns to the list", async () => {
  const h = app();
  await h.instance.start();
  await h.gate.last("showPcList").onPair({ pc: PC_ID, code: "7K3QDM9X2P" });
  h.pairings[0].onRefused({ code: 1008 });
  assert.equal(h.gate.last("showPairing").state, "failed");
  assert.equal(h.gate.last("showPairing").message,
    "The relay did not accept this code. Check the PC ID and code, or open a new code on the PC.");
  await h.gate.last("showPcList").onPair({ pc: PC_ID, code: "7K3QDM9X2P" });
  h.gate.last("showPairing").onCancel();
  await flush();
  h.pairings[1].onClosed({ code: 1000 });
  assert.equal(h.gate.calls.at(-1)[0], "showPcList");
});

test("list rename, remove, forget and lock call the store, fetch and reload", async () => {
  const fetches = [];
  const h = app({ pcs: [{ id: PC_ID, name: "Desk", public: PUBLIC, paired: 1, lastUsed: null }],
    fetch: async (url, options) => { fetches.push([url, options]); return { ok: true }; } });
  await h.instance.start();
  await h.gate.last("showPcList").onRename(PC_ID, "Office");
  assert.deepEqual(h.store.log.at(-1), ["rename", PC_ID, "Office"]);
  assert.equal(h.gate.last("showPcList").pcs[0].name, "Office");
  await h.gate.last("showPcList").onRemove(PC_ID);
  assert.deepEqual(h.store.log.at(-1), ["remove", PC_ID]);
  assert.deepEqual(h.gate.last("showPcList").pcs, []);
  await h.gate.last("showPcList").onForgetBrowser();
  assert.deepEqual(fetches, [[FORGET_ENDPOINT, { method: "POST", credentials: "same-origin" }]]);
  assert.equal(FORGET_ENDPOINT, "/__gate/forget");
  assert.equal(h.location.reloads, 1);
  h.gate.last("showPcList").onLock();
  assert.equal(h.location.reloads, 2);
});

test("a failed forget rejects for the gate and does not reload", async () => {
  const h = app({ fetch: async () => ({ ok: false, status: 503 }) });
  await h.instance.start();
  await assert.rejects(h.gate.last("showPcList").onForgetBrowser(), /could not be forgotten/);
  const h2 = app({ fetch: async () => { throw new TypeError("offline"); } });
  await h2.instance.start();
  await assert.rejects(h2.gate.last("showPcList").onForgetBrowser(), /could not be forgotten/);
  assert.equal(h.location.reloads + h2.location.reloads, 0);
});

test("connect from the list starts the session, then lock ends it before reload", async () => {
  const h = app({ pcs: [{ id: PC_ID, name: "Desk", public: PUBLIC, paired: 1, lastUsed: null }] });
  await h.instance.start();
  await h.gate.last("showPcList").onConnect(PC_ID);
  assert.equal(h.sessions[0].pc, PC_ID);
  assert.equal(new URL(h.location.href).searchParams.get("pc"), PC_ID);
  assert.equal(h.gate.last("showBusy"), "Connecting to Desk…");
  h.instance.lock();
  assert.deepEqual(h.sessions.at(-1), "ended");
  h.instance.switchPc();
  assert.deepEqual(h.location.assigned, ["https://relay.test/"]);
});

test("use a different key clears the stored key and returns to setup", async () => {
  const h = app({ key: PROTECTED });
  await h.instance.start();
  await h.gate.last("showKeyScreen").onRemove();
  assert.deepEqual(h.store.log, [["clear"]]);
  assert.equal(h.gate.last("showKeyScreen").mode, "setup");
});

test("an unreadable stored key offers setup with removal", async () => {
  const h = app({ key: "{not json" });
  await h.instance.start();
  const screen = h.gate.last("showKeyScreen");
  assert.equal(screen.mode, "setup");
  assert.equal(typeof screen.onRemove, "function");
});

// ---- One-time move of a stored legacy key to the shared format.
const LEGACY_PLAIN_TEXT = JSON.stringify({ format: "bubbleplay-device-key", id: VIEWER_ID, label: "Test", kdf: null, aead: null });
const LEGACY_PROTECTED_TEXT = JSON.stringify({ format: "bubbleplay-device-key", id: VIEWER_ID, label: "Test", kdf: { alg: "argon2id" }, aead: { alg: "aes-256-gcm" } });
const SHARED_PLAIN_TEXT = JSON.stringify({ format: SHARED_NAME, id: VIEWER_ID, label: "Test", kdf: null, aead: null });

// Collects console.warn lines while fn runs.
async function warnings(fn) {
  const lines = [];
  const warn = console.warn;
  console.warn = (...args) => lines.push(args.join(" "));
  try { await fn(); } finally { console.warn = warn; }
  return lines;
}

test("a stored legacy plain key is converted on boot, read back, and its backup dropped", async () => {
  const h = app({ key: LEGACY_PLAIN_TEXT });
  await h.instance.start();
  assert.equal(await h.store.load(), SHARED_PLAIN_TEXT);
  assert.equal(h.store.backup, null);
  assert.ok(h.gate.calls.some(([name]) => name === "showPcList"), "the app still routes to the PC list");
});

test("a stored key already in the shared format is left alone", async () => {
  let converts = 0;
  const h = app({ key: SHARED_PLAIN_TEXT, convert: async () => { converts++; return SHARED_PLAIN_TEXT; } });
  await h.instance.start();
  assert.equal(converts, 0);
  assert.equal(await h.store.load(), SHARED_PLAIN_TEXT);
  assert.equal(h.store.backup, null);
});

test("a stored legacy protected key converts at the next successful unlock, not before", async () => {
  const h = app({ key: LEGACY_PROTECTED_TEXT });
  await h.instance.start();
  assert.equal(await h.store.load(), LEGACY_PROTECTED_TEXT, "nothing changes before the passphrase");
  await assert.rejects(h.gate.last("showKeyScreen").onUnlock("wrong"), { code: "wrong-passphrase" });
  assert.equal(await h.store.load(), LEGACY_PROTECTED_TEXT, "a failed unlock converts nothing");
  await h.gate.last("showKeyScreen").onUnlock("right");
  assert.equal(JSON.parse(await h.store.load()).format, SHARED_NAME);
  assert.equal(h.store.backup, null);
});

test("a failed conversion keeps the old stored text, logs one line and uses the old key this session", async () => {
  const h = app({ key: LEGACY_PLAIN_TEXT, convert: async () => { throw Object.assign(new Error("secret detail"), { code: "invalid-file" }); } });
  let routed = false;
  const lines = await warnings(async () => { await h.instance.start(); routed = h.gate.calls.some(([name]) => name === "showPcList"); });
  assert.equal(await h.store.load(), LEGACY_PLAIN_TEXT);
  assert.equal(h.store.backup, null);
  assert.equal(routed, true, "the app keeps working on the old key");
  assert.equal(lines.length, 1);
  assert.match(lines[0], /convert: invalid-file/);
  assert.ok(!lines[0].includes(VIEWER_ID) && !lines[0].includes("secret detail"), "the log line carries no key material");
});

test("a read-back that does not match the old ID restores the old text and drops the backup", async () => {
  const unlock = async text => {
    if (text === SHARED_PLAIN_TEXT) return { id: "77VH-7M56-8GJK-Y12J", publicKey: IDENTITY_KEY.publicKey, privateKey: {} };
    return IDENTITY_KEY;
  };
  const h = app({ key: LEGACY_PLAIN_TEXT, unlock });
  const lines = await warnings(() => h.instance.start());
  assert.equal(await h.store.load(), LEGACY_PLAIN_TEXT);
  assert.equal(h.store.backup, null);
  assert.equal(lines.length, 1);
  assert.match(lines[0], /read back: identity-mismatch/);
});

test("a failed write leaves the old record in place and drops the backup", async () => {
  const h = app({ key: LEGACY_PLAIN_TEXT });
  const store = h.store;
  const save = store.save;
  store.save = async text => {
    if (text === SHARED_PLAIN_TEXT) throw new Error("quota exceeded");
    return save.call(store, text);
  };
  const lines = await warnings(() => h.instance.start());
  assert.equal(await store.load(), LEGACY_PLAIN_TEXT);
  assert.equal(store.backup, null);
  assert.equal(lines.length, 1);
  assert.match(lines[0], /write: error/);
});

test("a backup write that fails stops before the record is touched", async () => {
  const h = app({ key: LEGACY_PLAIN_TEXT });
  h.store.saveBackup = async () => { throw new Error("blocked"); };
  let saved = 0;
  const save = h.store.save;
  h.store.save = async text => { saved++; return save.call(h.store, text); };
  const lines = await warnings(() => h.instance.start());
  assert.equal(saved, 0);
  assert.equal(await h.store.load(), LEGACY_PLAIN_TEXT);
  assert.match(lines[0], /backup: error/);
});

// A backup left by a crash between writing it and clearing it (recoverBackup, run first on boot).
// These texts carry id and public, as real key files do; recoverBackup compares those two fields.
const KEY_FIELDS = { id: VIEWER_ID, public: PUBLIC };
const plainKey = (format, fields = KEY_FIELDS) => JSON.stringify({ format, ...fields, label: "Test", kdf: null, aead: null });
const protectedKey = (format, fields = KEY_FIELDS) => JSON.stringify({ format, ...fields, label: "Test", kdf: { alg: "argon2id" }, aead: { alg: "aes-256-gcm" } });
const LEGACY_PLAIN_KEY = plainKey("bubbleplay-device-key");
const SHARED_PLAIN_KEY = plainKey(SHARED_NAME);
const LEGACY_PROTECTED_KEY = protectedKey("bubbleplay-device-key");
const SHARED_PROTECTED_KEY = protectedKey(SHARED_NAME);

test("a backup of the same key is dropped on boot when the stored key is in the shared format and opens", async () => {
  const h = app({ store: memoryStore({ key: SHARED_PLAIN_KEY, backup: LEGACY_PLAIN_KEY }) });
  await h.instance.start();
  assert.equal(h.store.backup, null);
  assert.equal(await h.store.load(), SHARED_PLAIN_KEY, "the stored key is kept, not replaced by the backup");
  assert.ok(h.gate.calls.some(([name]) => name === "showPcList"));
});

test("a backup of the same key is dropped for a protected stored key, without asking for the passphrase first", async () => {
  const h = app({ store: memoryStore({ key: SHARED_PROTECTED_KEY, backup: LEGACY_PROTECTED_KEY }) });
  await h.instance.start();
  assert.equal(h.store.backup, null);
  assert.equal(await h.store.load(), SHARED_PROTECTED_KEY, "the stored key is kept, not replaced by the backup");
  assert.equal(h.gate.last("showKeyScreen").mode, "unlock");
});

test("a backup is restored as the stored key when the stored key is missing or unreadable, then converted", async () => {
  for (const key of [null, "{not json"]) {
    const h = app({ store: memoryStore({ key, backup: LEGACY_PLAIN_KEY }) });
    const lines = await warnings(() => h.instance.start());
    assert.equal(h.store.backup, null);
    assert.deepEqual(JSON.parse(await h.store.load()), JSON.parse(SHARED_PLAIN_KEY), "the backup came back and was then converted");
    assert.equal(lines.length, 1);
    assert.match(lines[0], /restored from its backup/);
  }
});

test("a backup with a different ID is kept beside the stored key, with one line and no key material", async () => {
  const OTHER_LEGACY = plainKey("bubbleplay-device-key", { id: PC_ID, public: "B".repeat(43) });
  const store = memoryStore({ key: SHARED_PLAIN_KEY, backup: OTHER_LEGACY });
  const h = app({ store });
  const lines = await warnings(() => h.instance.start());
  assert.equal(await store.load(), SHARED_PLAIN_KEY);
  assert.equal(store.backup, OTHER_LEGACY);
  assert.equal(lines.length, 1);
  assert.match(lines[0], /backup kept/);
  assert.ok(!lines[0].includes(VIEWER_ID) && !lines[0].includes(PC_ID), "the log line carries no IDs");
  assert.ok(h.gate.calls.some(([name]) => name === "showPcList"), "boot still uses the stored key");
});
