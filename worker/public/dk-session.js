// Endpoint authentication for one admitted signaling socket. The receiver owns sockets,
// peer connections and presentation; this module grants access only after the wire checks.
import { beginViewerHandshake } from "./dk-handshake.js";
import { parseSdpFingerprint, signBind, verifyBind } from "./dk-crypto.js";

export const VIEWER_CAPS = Object.freeze(["control-aead-v1", "dtls-bind-v1"]);
const AUTH_TIMEOUT_MS = 10_000;
const BINARY_KINDS = new Set(["cursor", "input"]);

export function browserDeviceName(nav = globalThis.navigator) {
  const ua = nav?.userAgent || "";
  const brands = nav?.userAgentData?.brands || [];
  const browser = brands.some(b => /Microsoft Edge/i.test(b.brand)) || /Edg\//.test(ua) ? "Edge"
    : brands.some(b => /Google Chrome/i.test(b.brand)) || /Chrome\//.test(ua) ? "Chrome"
    : /Firefox\//.test(ua) ? "Firefox" : /Safari\//.test(ua) ? "Safari" : "Browser";
  const platform = nav?.userAgentData?.platform || "";
  const os = /Android/i.test(platform + ua) ? "Android" : /iPhone|iPad|iPod/i.test(ua) ? "iOS"
    : /Windows/i.test(platform + ua) ? "Windows" : /Mac/i.test(platform + ua) ? "macOS"
    : /CrOS/i.test(ua) ? "ChromeOS" : /Linux/i.test(platform + ua) ? "Linux" : "";
  return (os ? `${browser} on ${os}` : browser).slice(0, 64);
}

export function sessionRefusal(reason, message = "") {
  if (reason === "pc-key-changed") return { title: "PC key changed", body: "This PC answered with a different key than the one this browser paired with. The session was closed before anything was sent. If the PC was reinstalled, remove it from the list and pair again." };
  if (reason === "auth-timeout") return { title: "PC not reachable", body: "This PC does not support device keys or is not reachable." };
  if (reason.startsWith("missing-cap:")) return { title: "PC needs an update", body: `This PC does not offer ${reason.slice(12)}. Update BubblePlay on the PC.` };
  if (reason === "dtls-bind-required" || reason === "cap-missing") {
    const body = reason === "dtls-bind-required"
      ? "The PC requires a verified WebRTC connection (dtls-bind-v1) that this browser did not offer."
      : "This browser lacks a capability the PC requires.";
    return { title: "PC refused the session", body: body + (typeof message === "string" && message ? ` ${message.slice(0, 200)}` : "") };
  }
  return { title: "Session ended", body: reason === "bind-failed"
    ? "The PC's connection could not be verified, so the session was closed."
    : "A control message failed its integrity check, so the session was closed." };
}

/** `send` must return false if the socket is no longer current. Call receive in wire order.
 * helloOptions and timers are test seams; production always generates fresh hello material.
 */
export function createViewerSession({ identity, pcId, pinnedPublic, relayOrigin, epoch,
  transport = "webrtc", name = browserDeviceName(), send, onAuthenticated = () => {},
  onMessage = () => {}, onBinary = () => {}, onMediaReady = () => {}, onFailure = () => {},
  schedule = setTimeout, cancel = clearTimeout, helloOptions }) {
  let ended = false, started = false, authenticated = false, capsReceived = false;
  let handshake = null, ctl = null, H = null, pcPublic = null;
  let timer = null, bindTimer = null, localBindSent = false, remoteBindVerified = false;
  let fingerprints = null, pendingBind = null, peerRevision = 0, bindWork = null;
  let readyNotified = false;
  let incoming = Promise.resolve();
  const mediaReady = () => !ended && capsReceived && (transport === "wss" || (localBindSent && remoteBindVerified));
  const sendWire = message => !ended && send(message) !== false;
  const clearTimers = () => { cancel(timer); cancel(bindTimer); timer = bindTimer = null; };
  function fail(reason = "control-failed", message) {
    if (ended) return;
    ended = true;
    clearTimers();
    ctl = handshake = null;
    onFailure(sessionRefusal(reason, message), reason);
  }
  function notifyReady() {
    if (!mediaReady() || readyNotified) return;
    readyNotified = true;
    cancel(bindTimer); bindTimer = null;
    onMediaReady();
  }
  async function sendSealed(message, binaryKind) {
    const channel = ctl;
    if (!channel || ended) return false;
    try {
      const envelope = binaryKind ? await channel.seal(binaryKind, message) : await channel.sealJson(message);
      if (ended || ctl !== channel) return false;
      if (!sendWire(envelope)) throw new Error("socket ended");
      return true;
    } catch { fail(); return false; }
  }
  async function checkBind(revision = peerRevision) {
    if (!fingerprints || !pendingBind || ended) return;
    const signature = pendingBind.signature;
    const { local, remote } = fingerprints;
    let valid = false;
    try { valid = await verifyBind(pcPublic, H, remote, local, signature); } catch {}
    if (ended || revision !== peerRevision) return;
    if (!valid) return fail("bind-failed");
    remoteBindVerified = true;
    notifyReady();
  }
  async function receiveNow(message) {
    if (ended) return;
    if (message?.type === "auth-host") {
      if (!handshake || authenticated) return fail();
      // The deadline is for receiving auth-host. Verification may yield after
      // a reply that arrived just before the ten-second boundary.
      cancel(timer); timer = null;
      try {
        const result = await handshake.handleAuthHost(message);
        if (ended) return;
        ({ H, pcPublic, ctl } = result);
        authenticated = true;
        // No endpoint traffic can precede the viewer's caps at sequence zero.
        if (!await sendSealed({ type: "caps", caps: [...VIEWER_CAPS], name: String(name).replace(/[\x00-\x1f\x7f]/g, "").slice(0, 64) })) return;
        timer = schedule(() => fail("missing-cap:control-aead-v1"), AUTH_TIMEOUT_MS);
      } catch (error) { fail(error.reason === "pc-key-changed" ? error.reason : "control-failed"); }
      return;
    }
    if (message?.type === "auth-bind") {
      // An RTC attempt may finish its bind after the viewer selected WSS.
      if (transport === "wss") return;
      if (!authenticated || pendingBind ||
          Object.keys(message).sort().join(",") !== "signature,type" || typeof message.signature !== "string") return fail("bind-failed");
      pendingBind = message;
      await checkBind();
      return;
    }
    if (message?.type !== "ctl" || !ctl) return fail();
    try {
      if (BINARY_KINDS.has(message.kind)) {
        const bytes = await ctl.open(message);
        if (!ended && capsReceived && mediaReady()) onBinary(message.kind, bytes);
        else if (!capsReceived) fail();
        return;
      }
      const inner = await ctl.openJson(message);
      if (ended) return;
      if (inner.type === "error" && ["dtls-bind-required", "cap-missing"].includes(inner.code)) return fail(inner.code, inner.message);
      if (!capsReceived) {
        if (inner.type !== "caps" || !Array.isArray(inner.caps) || !inner.caps.every(cap => typeof cap === "string")) return fail();
        const required = transport === "wss" ? ["control-aead-v1"] : VIEWER_CAPS;
        const missing = required.find(cap => !inner.caps.includes(cap));
        if (missing) return fail(`missing-cap:${missing}`);
        capsReceived = true;
        cancel(timer); timer = null;
        onAuthenticated();
        notifyReady();
        return;
      }
      await onMessage(inner);
    } catch { fail(); }
  }
  return {
    get authenticated() { return authenticated && !ended; },
    get controlReady() { return capsReceived && !ended; },
    get mediaReady() { return mediaReady(); },
    get rtcReady() { return !ended && capsReceived && localBindSent && remoteBindVerified; },
    async start() {
      if (started || ended) return;
      started = true;
      if (!(pinnedPublic instanceof Uint8Array) || pinnedPublic.length !== 32 || typeof epoch !== "string" || !/^[\x20-\x7e]{1,64}$/.test(epoch)) return fail();
      timer = schedule(() => fail("auth-timeout"), AUTH_TIMEOUT_MS);
      try {
        handshake = await beginViewerHandshake({ identity, pcId, pinnedPublic, relayOrigin, epoch, helloOptions,
          send: message => { if (!sendWire(message)) throw new Error("socket ended"); } });
      } catch { fail(); }
    },
    receive(message) {
      const work = incoming.then(() => receiveNow(message));
      incoming = work.catch(() => fail());
      return incoming;
    },
    sendControl(message) {
      if (!capsReceived || ended) return false;
      void sendSealed(message);
      return true;
    },
    sendInput(bytes) {
      if (!mediaReady()) return false;
      const copy = bytes instanceof ArrayBuffer ? new Uint8Array(bytes.slice(0))
        : new Uint8Array(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength));
      void sendSealed(copy, "input");
      return true;
    },
    bindPeer({ localSdp, remoteSdp }) {
      if (ended || !capsReceived || transport === "wss") return Promise.resolve();
      if (bindWork) return bindWork;
      const revision = peerRevision;
      bindTimer = schedule(() => fail("bind-failed"), AUTH_TIMEOUT_MS);
      bindWork = (async () => {
        try {
          const local = parseSdpFingerprint(localSdp);
          const remote = parseSdpFingerprint(remoteSdp);
          if (!local || !remote) {
            // Decision 23: no sha-256 fingerprint ends the session with a ctl error to the PC.
            await sendSealed({ type: "error", code: "dtls-bind-required", message: "no sha-256 DTLS fingerprint in the SDP" });
            if (revision === peerRevision) fail("bind-failed");
            return;
          }
          fingerprints = { local, remote };
          const signature = await signBind(identity.privateKey, H, fingerprints.local, fingerprints.remote);
          if (ended || revision !== peerRevision) return;
          if (!sendWire({ type: "auth-bind", signature })) return fail("bind-failed");
          localBindSent = true;
          await checkBind(revision);
          notifyReady();
        } catch { if (revision === peerRevision) fail("bind-failed"); }
      })();
      return bindWork;
    },
    resetPeer() {
      peerRevision++;
      fingerprints = pendingBind = bindWork = null;
      localBindSent = remoteBindVerified = readyNotified = false;
      cancel(bindTimer); bindTimer = null;
    },
    useWss() {
      transport = "wss";
      cancel(bindTimer); bindTimer = null;
      notifyReady();
    },
    close() { ended = true; clearTimers(); ctl = handshake = null; },
  };
}
