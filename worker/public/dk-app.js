// Browser viewer entry: nothing connects until this browser's device key is unlocked.
// Boot: load the stored key; use the kept unlocked key when it matches, else unlock it
// (plain: at once; protected: passphrase) and keep the result; then route:
//   /pair#pc=<id>&code=<code>  -> pairing (the fragment is dropped from the URL at once)
//   ?pc=<id> naming a stored PC -> connect
//   otherwise                   -> the PC list
// UI: dk-gate.js (screens only). Session: receiver.js startSession. Protocol: dk-relay.js.
// The unlocked Ed25519 private key is a non-extractable CryptoKey. It is kept in IndexedDB
// (devicekey-store.js) so a refresh needs no passphrase; Lock this browser deletes it.
// Spec: docs/architecture/device-keys.md.
import { canonicalId, isOtherFormatKeyFileText, unlockKeyFile } from "./devicekey.js";
import { createDeviceKeyStore } from "./devicekey-store.js";
import { deviceKeyErrorMessage, passphraseValue, summarizeKeyFile } from "./devicekey-ui.js";
import { normalizePairingCode, openPairingSocket, pairAdmitHash, pcListUrl, relayOrigin } from "./dk-relay.js";
import { b64u, pairKey, pairMac, verifyPairMac, comparisonNumber } from "./dk-crypto.js";
import { beginViewerHandshake } from "./dk-handshake.js";
import { installFeedback } from "./dk-feedback.js";

export const FORGET_ENDPOINT = "/__gate/forget";

// Local viewer log (dk-log.js). Absent in tests and when the script is blocked. Callers pass
// plain causes (error class and message), never passphrases, codes, keys or key file text.
const blog = (level, msg, fields) => { try { globalThis.BPLog?.log(level, "dk-app", msg, fields); } catch { /* logging never breaks the viewer */ } };
const setLogContext = context => { try { globalThis.BPLog?.setContext(context); } catch { /* ignore */ } };
const hideSecret = secret => { try { if (secret) globalThis.BPLog?.registerSecret(String(secret)); } catch { /* ignore */ } };
const cause = error => (error === undefined || error === null ? undefined
  : `${error.name || "Error"}: ${String(error.message ?? error)}`.slice(0, 160));

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
  beginHandshake = beginViewerHandshake,
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  now = Date.now,
}) {
  let identity = null;
  let session = null;
  let pendingLink = null;
  let pairingAttempt = null;

  function stopPairing() {
    pairingAttempt?.stop();
    pairingAttempt = null;
  }

  function wrongPassphrase() {
    const error = new Error("Wrong passphrase.");
    error.code = "wrong-passphrase";
    return error;
  }

  async function unlockWith(text, passphrase) {
    hideSecret(passphrase);
    try {
      const key = await unlock(text, passphrase);
      identity = Object.freeze({ id: key.id, publicRaw: key.publicKey, privateKey: key.privateKey });
      setLogContext({ device: key.id });
    } catch (error) {
      identity = null;
      blog("warn", "key unlock failed", { cause: error?.code === "authentication-failed" ? "wrong passphrase" : cause(error) });
      if (error?.code === "authentication-failed") throw wrongPassphrase();
      const shown = new Error(deviceKeyErrorMessage(error));
      shown.code = error?.code ?? "unlock-failed";
      throw shown;
    }
    // Keep it for the next visit. The store refuses an extractable key; if the browser cannot
    // store a CryptoKey at all, the passphrase is simply asked again next time.
    try { await store.saveUnlocked(identity); } catch (error) {
      blog("warn", "could not keep the key unlocked", { cause: cause(error) }); /* not kept */
    }
  }

  /** The kept unlocked key, if it belongs to this key file; a stale one is deleted. */
  async function keptIdentity(file) {
    let kept = null;
    try { kept = await store.loadUnlocked(); } catch (error) {
      blog("warn", "could not read the kept unlocked key", { cause: cause(error) });
      return null;
    }
    if (!kept) return null;
    if (kept.id === file.id && b64u.encode(kept.publicRaw) === file.public) {
      setLogContext({ device: kept.id });
      return Object.freeze(kept);
    }
    blog("info", "kept unlocked key belongs to another key file; clearing it");
    try { await store.clearUnlocked(); } catch (error) {
      blog("warn", "could not clear the stale unlocked key", { cause: cause(error) }); /* next boot retries */
    }
    return null;
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
    stopPairing();
    identity = null;
    const text = await store.load();
    if (text === null || text === undefined) return showSetup(false);
    // A stored key in any other format counts as no key: setup replaces it.
    if (isOtherFormatKeyFileText(text)) return showSetup(false);
    let info = null, file = null;
    let parseError = null;
    try { file = JSON.parse(text); info = summarizeKeyFile(file); } catch (error) { parseError = error; /* unreadable: offer replacement */ }
    if (!info) {
      blog("warn", "stored key file is unreadable", { cause: cause(parseError) ?? "key file summary is empty" });
      return showSetup(true);
    }
    const kept = await keptIdentity(file);
    if (kept) {
      identity = kept;
      return route();
    }
    if (!info.protected) {
      gate.showBusy("Unlocking key…");
      await unlockWith(text, undefined);
      return route();
    }
    gate.showKeyScreen({
      mode: "unlock",
      onUnlock: async passphrase => {
        const value = passphraseValue(passphrase ?? "");
        await unlockWith(text, value);
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
        blog("warn", "pairing link rejected", { cause: cause(error) });
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
    stopPairing();
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
    stopPairing();
    const record = await store.getPc(id);
    if (!record) return showList();
    const url = new URL(location.href);
    url.searchParams.set("pc", record.id);
    url.hash = "";
    history.replaceState(null, "", `${url.pathname}${url.search}`);
    gate.showBusy(`Connecting to ${record.name}…`);
    setLogContext({ pc: record.id });
    blog("info", "connecting to a PC");
    session = await loadSession();
    await session.startSession({ identity, pc: record.id, pinnedPublic: record.public, gate });
  }

  /** Pair at the PC: prove the code and PC identity before showing a comparison number. */
  async function startPairing(input) {
    const { pc, code } = normalizePairingInput(input);
    // The code is typed by hand or comes from a link; keep every form of it out of the log.
    hideSecret(code); hideSecret(String(input.code ?? "").trim()); hideSecret(`${code.slice(0, 5)}-${code.slice(5)}`);
    setLogContext({ pc });
    blog("info", "pairing started");
    stopPairing();
    let socket = null;
    let ended = false;
    let timer = null;
    let stage = "admission";
    let handshake = null;
    let verified = null;
    let key = null;
    let queue = Promise.resolve();
    const attempt = { stop() {
      ended = true;
      clearTimer(timer);
      key = null;
      socket?.close();
    } };
    pairingAttempt = attempt;
    const active = () => !ended && pairingAttempt === attempt;
    const fail = (message, state = "failed") => {
      if (!active()) return;
      attempt.stop();
      blog("warn", "pairing ended", { outcome: state, shown: message });
      gate.showPairing({ state, pcId: pc, message, onBack: showList });
    };
    const closedMessage = "The PC closed the pairing window. Open Pair device on the PC again.";
    const notReachable = () => {
      if (!active()) return;
      attempt.stop();
      blog("warn", "pairing timed out", { stage });
      gate.showRefusal({ title: "PC not reachable", body: "This PC does not support device keys or is not reachable.",
        actionLabel: "Back to PCs", onAction: async () => {
          const url = new URL(pcListUrl(location.href));
          history.replaceState(null, "", `${url.pathname}${url.search}`);
          await showList();
        } });
    };
    const deadline = (milliseconds, onTimeout = notReachable) => {
      clearTimer(timer);
      timer = setTimer(onTimeout, milliseconds);
      timer?.unref?.();
    };
    const cancel = async () => { attempt.stop(); await showList(); };
    const send = message => {
      if (active()) socket.send(JSON.stringify(message));
    };
    // Even injected socket adapters must preserve the asynchronous handshake's message order.
    const inOrder = task => {
      queue = queue.then(async () => { if (active()) await task(); }).catch(error => {
        blog("warn", "pairing exchange failed", { stage, cause: cause(error) });
        fail(error?.reason === "pc-key-changed"
          ? "This PC answered with a different key. Remove it from the PC list and pair again."
          : "The PC's pairing exchange could not be verified. Open a new pairing code on the PC and try again.");
      });
      return queue;
    };
    gate.showPairing({ state: "connecting", pcId: pc, onCancel: cancel });
    deadline(10000);
    try {
      const pair = await pairAdmitHash(pc, code);
      key = await pairKey(code);
      if (!active()) { key = null; return; }
      socket = openPairing({
        identity, pc, pair, href: location.href, origin: relayOrigin(location.href),
        onPairingAdmitted: ({ socket: admittedSocket, message }) => inOrder(async () => {
          socket = admittedSocket;
          if (stage !== "admission" || message.pc !== pc || message.pairing !== true || message.ticket !== undefined ||
              typeof message.mediaEpoch !== "string" || !/^[\x20-\x7e]{1,64}$/.test(message.mediaEpoch))
            throw new Error("Invalid pairing admission");
          stage = "auth-host";
          deadline(10000);
          handshake = await beginHandshake({
            identity, pcId: pc, pinnedPublic: null,
            relayOrigin: relayOrigin(location.href), epoch: message.mediaEpoch, pairing: true, send,
          });
        }),
        onMessage: message => inOrder(async () => {
          if (stage === "done") return;
          if (message.type === "auth-host" && stage === "auth-host") {
            clearTimer(timer);
            verified = await handshake.handleAuthHost(message);
            if (!active()) return;
            const mac = await pairMac(key, "viewer", verified.H);
            if (!active()) return;
            stage = "pc-proof";
            send({ type: "pair-proof", mac });
            deadline(10000);
          } else if (message.type === "pair-proof" && stage === "pc-proof") {
            if (Object.keys(message).sort().join(",") !== "mac,type" ||
                !await verifyPairMac(key, "pc", verified.H, message.mac)) {
              fail("The pairing code did not match.");
              return;
            }
            const number = await comparisonNumber(verified.H);
            if (!active()) return;
            key = null;
            stage = "approval";
            deadline(300000, () => fail(closedMessage));
            gate.showPairing({ state: "compare", pcId: pc, number, onCancel: cancel });
          } else if (message.type === "pair-done" && stage === "approval" && Object.keys(message).length === 1) {
            const publicText = b64u.encode(verified.pcPublic);
            stage = "done";
            clearTimer(timer);
            let saved = false;
            gate.showPairing({ state: "done", pcId: pc, onSave: async name => {
              if (!active() || saved) return;
              saved = true;
              try { await store.putPc({ id: pc, public: publicText, name, paired: now(), lastUsed: null }); }
              catch (error) { saved = false; throw error; }
              if (!active()) return;
              attempt.stop();
              await connect(pc);
            } });
          } else if (message.type === "pair-denied" && Object.keys(message).length === 1) {
            fail(undefined, "denied");
          } else if (["auth-host", "auth-finish", "auth-bind", "pair-proof", "pair-done", "ctl"].includes(message.type)) {
            throw new Error("Unexpected pairing message");
          } else if (["pair-closed", "peer-left", "error"].includes(message.type)) {
            fail(closedMessage);
          }
        }),
        onRefused: event => {
          // The relay may put a short reason in the 1008 close; show it when present.
          const said = typeof event?.reason === "string" ? event.reason.replace(/[^\x20-\x7e]/g, "").trim().slice(0, 120) : "";
          blog("warn", "pairing refused by the relay", { closeCode: event?.code, reason: said });
          fail(`The relay did not accept this code. Check the PC ID and code, or open a new code on the PC.${said ? ` Reason: ${said}.` : ""}`);
        },
        onClosed: (event, info) => {
          if (stage === "done") return;
          blog("warn", "pairing socket closed", { closeCode: event?.code, reason: String(event?.reason ?? "").slice(0, 120), admitted: info?.admitted, stage });
          fail(closedMessage);
        },
        onError: event => {
          if (stage === "done") return;
          blog("warn", "pairing socket failed", { stage, cause: cause(event instanceof Error ? event : undefined) ?? "socket error" });
          fail("The pairing connection failed. Check the connection and try again.");
        },
      });
      return { pc, code, pair };
    } catch (error) {
      blog("warn", "pairing could not start", { cause: cause(error) });
      fail(error.message);
    }
  }

  async function forgetBrowser() {
    const problem = new Error("This browser could not be forgotten. Check the connection and try again.");
    let response;
    try { response = await fetch(FORGET_ENDPOINT, { method: "POST", credentials: "same-origin" }); }
    catch (error) { blog("warn", "forget this browser: request failed", { cause: cause(error) }); throw problem; }
    if (!response.ok) { blog("warn", "forget this browser: refused", { status: response.status }); throw problem; }
    stopPairing();
    session?.endSession?.();
    reload();
  }

  function switchPc() {
    stopPairing();
    session?.endSession?.();
    location.assign(pcListUrl(location.href));
  }

  /** Lock this browser: end the session, delete the kept unlocked key, then start over. */
  async function lock() {
    stopPairing();
    session?.endSession?.();
    await store.clearUnlocked();
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
  try { installFeedback(doc); } catch (error) { blog("warn", "report a problem is unavailable", { cause: cause(error) }); }
  try {
    await app.start();
  } catch (error) {
    blog("error", "viewer start failed", { cause: cause(error), stack: String(error?.stack ?? "") });
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
  on("lock-key", async () => {
    try { await app.lock(); }
    catch (error) {
      blog("warn", "lock this browser failed", { cause: cause(error) });
      if (status) status.textContent = "This browser could not be locked. Close other tabs and try again.";
    }
  });
  on("forget-browser", async () => {
    if (!globalThis.confirm("Forget this browser? The next visit will require new browser approval.")) return;
    if (status) status.textContent = "Forgetting this browser…";
    try { await app.forgetBrowser(); }
    catch (error) { if (status) status.textContent = error.message; }
  });
}

if (typeof window !== "undefined" && typeof document !== "undefined") main();
