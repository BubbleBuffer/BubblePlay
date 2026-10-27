// Phase-2 relay protocol for the browser viewer: the signed viewer hello on /ws,
// the relay proof, the pairing admit hash and the pairing-mode socket.
// Spec: docs/architecture/device-keys.md, "Phase 2 relay protocol" and
// "Encodings for implementers" (Relay proof). No DOM; safe to import in Node.

import { normalizePairCode, pairAdmitHash as cryptoPairAdmitHash } from "./dk-crypto.js";

const encoder = new TextEncoder();
export const PAIR_CODE_LENGTH = 10;

export function b64urlEncode(bytes) {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Strict unpadded base64url; returns null for anything else or a wrong length. */
export function b64urlDecode(text, length) {
  if (typeof text !== "string" || !/^[A-Za-z0-9_-]*$/.test(text) || text.length % 4 === 1) return null;
  let binary;
  try { binary = atob(text.replace(/-/g, "+").replace(/_/g, "/")); } catch { return null; }
  const bytes = Uint8Array.from(binary, char => char.charCodeAt(0));
  if (length !== undefined && bytes.length !== length) return null;
  // Non-canonical trailing bits would decode to the same bytes; refuse them.
  if (b64urlEncode(bytes) !== text) return null;
  return bytes;
}

/**
 * The relay origin the proof signs, canonicalised like relay-rs (bp-device-key proof.rs
 * relay_origin): https for https/wss, http for http/ws, host lowercased, default port dropped.
 * Returns null for URLs without a usable scheme or host, or with userinfo.
 */
export function relayOrigin(href) {
  let url;
  try { url = new URL(href); } catch { return null; }
  const scheme = { "https:": "https", "wss:": "https", "http:": "http", "ws:": "http" }[url.protocol];
  if (!scheme || url.username || url.password || !url.hostname) return null;
  const defaultPort = scheme === "https" ? "443" : "80";
  const port = url.port && url.port !== defaultPort ? `:${url.port}` : "";
  return `${scheme}://${url.hostname.toLowerCase()}${port}`;
}

function lengthPrefixed(parts) {
  const chunks = parts.map(part => typeof part === "string" ? encoder.encode(part) : part);
  const out = new Uint8Array(chunks.reduce((sum, chunk) => sum + 4 + chunk.length, 0));
  const view = new DataView(out.buffer);
  let offset = 0;
  for (const chunk of chunks) {
    view.setUint32(offset, chunk.length);
    out.set(chunk, offset + 4);
    offset += 4 + chunk.length;
  }
  return out;
}

/** Relay-proof encoding: each field uint32 BE length || bytes. */
export function relayProofEncoding({ role, origin, nonce, id, publicRaw }) {
  return lengthPrefixed(["bubbleplay-relay-v1", role, origin, nonce, id, publicRaw]);
}

/** H = SHA-256(encoding); Ed25519 signs these 32 bytes directly. */
export async function relayProofHash(fields) {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", relayProofEncoding(fields)));
}

/** The register-proof reply to a register-challenge, or null when the nonce is malformed. */
export async function registerProof({ identity, origin, nonce: nonceText, role = "viewer" }) {
  const nonce = b64urlDecode(nonceText, 32);
  if (!nonce || !origin) return null;
  const hash = await relayProofHash({ role, origin, nonce, id: identity.id, publicRaw: identity.publicRaw });
  const signature = new Uint8Array(await crypto.subtle.sign({ name: "Ed25519" }, identity.privateKey, hash));
  return { type: "register-proof", signature: b64urlEncode(signature) };
}

/** The viewer hello: exactly type, role, id, public and optionally turn and pair. */
export function viewerHello({ identity, turn = false, pair = null }) {
  return {
    type: "hello",
    role: "viewer",
    id: identity.id,
    public: b64urlEncode(identity.publicRaw),
    ...(turn ? { turn: true } : {}),
    ...(pair ? { pair } : {}),
  };
}

/** Pairing code: uppercase, O->0, I/L->1, dashes and spaces dropped; 10 Crockford characters. */
export function normalizePairingCode(input) {
  try { return normalizePairCode(input); } catch { return null; }
}

/** b64url(SHA-256("bubbleplay-pair-admit-v1" || canonical PC ID || normalised code)). */
export async function pairAdmitHash(pcId, code) {
  return cryptoPairAdmitHash(pcId, code);
}

/** The page URL that shows the PC list: the current URL without pc, path "/", no fragment. */
export function pcListUrl(href) {
  const url = new URL(href);
  url.pathname = "/";
  url.searchParams.delete("pc");
  url.hash = "";
  return url.href;
}

/** wss://host/ws?pc=<id> for the page at href. */
export function signalingUrl(href, pcId) {
  const url = new URL(href);
  const proto = url.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${url.host}/ws?pc=${encodeURIComponent(pcId)}`;
}

/**
 * Opens /ws in pairing mode: hello with `pair`, register-proof, then hello-ok with pairing:true.
 * After admission, onMessage receives pairing messages in socket order. Callback failures close
 * the socket and report onError. There are no session/media/control hooks on this socket.
 */
export function openPairingSocket({
  identity, pc, pair, href, origin = relayOrigin(href), WebSocketImpl = globalThis.WebSocket,
  onPairingAdmitted, onMessage, onRefused, onClosed, onError,
}) {
  const socket = new WebSocketImpl(signalingUrl(href, pc));
  let admitted = false;
  let challenged = false;
  let closed = false;
  let transportClosed = false;
  let queue = Promise.resolve();
  const send = message => { if (!closed && !transportClosed) socket.send(JSON.stringify(message)); };
  socket.onopen = () => send(viewerHello({ identity, pair }));
  async function receive(event) {
    if (closed) return;
    if (typeof event.data !== "string") return;
    let message;
    try { message = JSON.parse(event.data); } catch { return; }
    if (message.type === "register-challenge" && !challenged && !admitted) {
      challenged = true;
      const proof = await registerProof({ identity, origin, nonce: message.nonce });
      if (proof) send(proof);
      else socket.close();
    } else if (message.type === "hello-ok" && !admitted) {
      if (message.pairing !== true) { socket.close(); return; }
      admitted = true;
      await onPairingAdmitted?.({ socket, message });
    } else if (admitted) {
      await onMessage?.(message);
    }
  }
  socket.onmessage = event => {
    queue = queue.then(() => receive(event)).catch(error => {
      closed = true;
      socket.close();
      onError?.(error);
    });
    return queue;
  };
  socket.onclose = event => {
    // A close event may arrive while WebCrypto is still checking earlier message events.
    // Drain those messages first: pair-done/denied received before close must win over close.
    transportClosed = true;
    queue = queue.then(() => {
      closed = true;
      if (!admitted && event.code === 1008) onRefused?.(event);
      else onClosed?.(event, { admitted });
    });
    return queue;
  };
  socket.onerror = event => onError?.(event);
  return socket;
}
