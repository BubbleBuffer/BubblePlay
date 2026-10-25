// Browser viewer entry: nothing connects until this browser's device key is unlocked.
// Boot: load the stored key, unlock it (plain: at once; protected: passphrase), then route:
//   /pair#pc=<id>&code=<code>  -> pairing (the fragment is dropped from the URL at once)
//   ?pc=<id> naming a stored PC -> connect
//   otherwise                   -> the PC list
// UI: dk-gate.js (screens only). Session: receiver.js startSession. Protocol: dk-relay.js.
// The unlocked Ed25519 private key is a non-extractable CryptoKey held in memory only.
// Spec: docs/architecture/device-keys.md.
import { canonicalId, unlockKeyFile } from "./devicekey.js";
import { createDeviceKeyStore } from "./devicekey-store.js";
import { deviceKeyErrorMessage, passphraseValue, summarizeKeyFile } from "./devicekey-ui.js";
import { normalizePairingCode, openPairingSocket, pairAdmitHash, pcListUrl, relayOrigin } from "./dk-relay.js";

export const FORGET_ENDPOINT = "/__gate/forget";

/** Newest use first; never-used PCs after, by name. */
export function sortPcs(pcs) {
  return [...pcs].sort((a, b) =>
    (b.lastUsed ?? -1) - (a.lastUsed ?? -1) || String(a.name).localeCompare(String(b.name)) || a.id.localeCompare(b.id));
}

/** Validates a typed or scanned PC ID and pairing code; throws Error(message) for the gate. */
export function normalizePairingInput({ pc, code }) {
  let id;
  try { id = canonicalId(String(pc ?? "").trim()); }
  catch { throw new Error("That PC ID is not valid. It has 16 characters, such as B7QK-2M9X-H4PD-R8TN."); }
  const normalized = normalizePairingCode(code);
  if (!normalized) throw new Error("That pairing code is not valid. It has 10 characters, such as 7K3QD-M9X2P.");
  return { pc: id, code: normalized };
}

/** Reads the /pair fragment. Returns null when this is not a pairing link. */
export function readPairingLink(href) {
  const url = new URL(href);
  if (url.pathname !== "/pair") return null;
  const fragment = new URLSearchParams(url.hash.replace(/^#/, ""));
  return { pc: fragment.get("pc") ?? "", code: fragment.get("code") ?? "" };
}

export function createApp({
  gate,
  store = createDeviceKeyStore(),
  unlock = unlockKeyFile,
  keyUi = null,                      // { createKey, importKey } from devicekey-ui.js installDeviceKeyUi
  loadSession,                       // async () => receiver.js module ({ startSession, endSession })
  location = globalThis.location,
  history = globalThis.history,
  fetch = globalThis.fetch?.bind(globalThis),
  reload = () => location.reload(),
  openPairing = openPairingSocket,
}) {
  let identity = null;
  let session = null;
  let pendingLink = null;

  function wrongPassphrase() {
    const error = new Error("Wrong passphrase.");
    error.code = "wrong-passphrase";
    return error;
  }

  async function unlockWith(text, passphrase) {
    try {
      const key = await unlock(text, passphrase);
      identity = Object.freeze({ id: key.id, publicRaw: key.publicKey, privateKey: key.privateKey });
    } catch (error) {
      if (error?.code === "authentication-failed") throw wrongPassphrase();
      const shown = new Error(deviceKeyErrorMessage(error));
      shown.code = error?.code ?? "unlock-failed";
      throw shown;
    }
  }

  async function afterKeyFlow(flow) {
    await flow();
    await boot();
  }

  async function removeKey() {
    await store.clear();
    await boot();
  }

  function showSetup(unreadable) {
    gate.showKeyScreen({
      mode: "setup",
      onCreate: keyUi ? () => afterKeyFlow(keyUi.createKey) : undefined,
      onImport: keyUi ? () => afterKeyFlow(keyUi.importKey) : undefined,
      onRemove: unreadable ? removeKey : undefined,
    });
  }

  /** Loads and unlocks the key, then routes. Re-entered after create, import or remove. */
  async function boot() {
    identity = null;
    const text = await store.load();
    if (text === null || text === undefined) return showSetup(false);
    let info = null;
    try { info = summarizeKeyFile(JSON.parse(text)); } catch { /* unreadable: offer replacement */ }
    if (!info) return showSetup(true);
    if (!info.protected) {
      gate.showBusy("Unlocking key…");
      await unlockWith(text, undefined);
      return route();
    }
    gate.showKeyScreen({
      mode: "unlock",
      onUnlock: async passphrase => {
        await unlockWith(text, passphraseValue(passphrase ?? ""));
        await route();
      },
      onCreate: undefined,
      onImport: undefined,
      onRemove: removeKey,
    });
  }

  /** Strips a pairing fragment from the address bar before anything else runs. */
  function takePairingLink() {
    const link = readPairingLink(location.href);
    if (!link) return;
    const url = new URL(location.href);
    history.replaceState(null, "", `${url.pathname}${url.search}`);
    pendingLink = link;
  }

  async function route() {
    const link = pendingLink;
    pendingLink = null;
    if (link) {
      try {
        return await startPairing(link);
      } catch (error) {
        gate.showPairing({ state: "failed", pcId: undefined, message: error.message, onBack: showList });
        return;
      }
    }
    const requested = new URL(location.href).searchParams.get("pc");
    if (requested) {
      const record = await store.getPc(requested);
      if (record) return connect(record.id);
    }
    return showList();
  }

  async function showList() {
    const pcs = sortPcs(await store.listPcs());
    gate.showPcList({
      deviceId: identity.id,
      pcs: pcs.map(({ id, name, lastUsed }) => ({ id, name, lastUsed: lastUsed ?? null })),
      onConnect: id => connect(id),
      onRename: async (id, name) => { await store.renamePc(id, name); await showList(); },
      onRemove: async id => { await store.removePc(id); await showList(); },
      onPair: input => startPairing(input),
      onForgetBrowser: forgetBrowser,
      onLock: lock,
    });
  }

  async function connect(id) {
    const record = await store.getPc(id);
    if (!record) return showList();
    const url = new URL(location.href);
    url.searchParams.set("pc", record.id);
    url.hash = "";
    history.replaceState(null, "", `${url.pathname}${url.search}`);
    gate.showBusy(`Connecting to ${record.name}…`);
    session = await loadSession();
    session.startSession({ identity, pc: record.id, gate });
  }

  /**
   * Pairing seam. This run: validate, compute the admit hash, open /ws in pairing mode and stop
   * at hello-ok pairing:true. The handshake, comparison number and putPc come in a later run.
   */
  async function startPairing(input) {
    const { pc, code } = normalizePairingInput(input);
    const pair = await pairAdmitHash(pc, code);
    let socket = null;
    let cancelled = false;
    const failed = () => { if (!cancelled) gate.showPairing({ state: "failed", pcId: pc, onBack: showList }); };
    gate.showPairing({ state: "connecting", pcId: pc, onCancel: () => { cancelled = true; socket?.close(); showList(); } });
    socket = openPairing({
      identity, pc, pair, href: location.href, origin: relayOrigin(location.href),
      onPairingAdmitted: () => {
        // TODO(dk2 handshake run): auth-hello / auth-host / auth-finish with pair-v1, pair-proof
        // both ways, gate.showPairing({ state: "compare", number }), pair-done -> store.putPc.
      },
      onRefused: failed,
      onClosed: failed,
    });
    return { pc, code, pair };
  }

  async function forgetBrowser() {
    const problem = new Error("This browser could not be forgotten. Check the connection and try again.");
    let response;
    try { response = await fetch(FORGET_ENDPOINT, { method: "POST", credentials: "same-origin" }); }
    catch { throw problem; }
    if (!response.ok) throw problem;
    session?.endSession?.();
    reload();
  }

  function switchPc() {
    session?.endSession?.();
    location.assign(pcListUrl(location.href));
  }

  function lock() {
    session?.endSession?.();
    reload();
  }

  async function start() {
    takePairingLink();
    await boot();
  }

  return { start, boot, route, showList, connect, startPairing, forgetBrowser, lock, switchPc, identity: () => identity };
}

/** Browser bootstrap: the one entry module of index.html. */
export async function main(doc = globalThis.document) {
  const [{ mountGate }, { installDeviceKeyUi }] = await Promise.all([import("/dk-gate.js"), import("/devicekey-ui.js")]);
  const gate = mountGate(doc);
  let receiver = null;
  const loadSession = async () => (receiver ??= await import("/receiver.js"));
  const keyUi = await installDeviceKeyUi({ doc });
  const app = createApp({ gate, keyUi, loadSession });
  wireSettings(doc, app);
  try {
    await app.start();
  } catch (error) {
    gate.showRefusal({ title: "Device key unavailable", body: deviceKeyErrorMessage(error) });
  }
}

// The settings panel's Browser access slot: Switch PC, Lock and Forget this browser.
function wireSettings(doc, app) {
  const status = doc.getElementById("session-action-status");
  const on = (id, handler) => {
    const button = doc.getElementById(id);
    if (button) button.onclick = event => { event.stopPropagation(); handler(); };
  };
  on("switch-pc", () => app.switchPc());
  on("lock-key", () => app.lock());
  on("forget-browser", async () => {
    if (!globalThis.confirm("Forget this browser? The next visit will require new browser approval.")) return;
    if (status) status.textContent = "Forgetting this browser…";
    try { await app.forgetBrowser(); }
    catch (error) { if (status) status.textContent = error.message; }
  });
}

if (typeof window !== "undefined" && typeof document !== "undefined") main();
