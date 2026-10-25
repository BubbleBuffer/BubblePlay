// Phase-2 relay protocol for the browser viewer: the signed viewer hello on /ws,
// the relay proof, the pairing admit hash and the pairing-mode socket.
// Spec: docs/architecture/device-keys.md, "Phase 2 relay protocol" and
// "Encodings for implementers" (Relay proof). No DOM; safe to import in Node.

const encoder = new TextEncoder();
const CROCKFORD = /^[0-9A-HJKMNP-TV-Z]+$/;
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
  const normalized = String(input ?? "").replace(/[\s-]/g, "").toUpperCase().replace(/O/g, "0").replace(/[IL]/g, "1");
  if (normalized.length !== PAIR_CODE_LENGTH || !CROCKFORD.test(normalized)) return null;
  return normalized;
}

/** b64url(SHA-256("bubbleplay-pair-admit-v1" || canonical PC ID || normalised code)). */
export async function pairAdmitHash(pcId, code) {
  const parts = ["bubbleplay-pair-admit-v1", pcId, code].map(text => encoder.encode(text));
  const bytes = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let offset = 0;
  for (const part of parts) { bytes.set(part, offset); offset += part.length; }
  return b64urlEncode(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)));
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
 * The end-to-end pairing handshake (auth-hello, pair-proof, ...) is added by a later run in
 * onPairingAdmitted; this run stops there. Any close before hello-ok is reported by onRefused
 * (code 1008 = not admitted) or onClosed.
 */
export function openPairingSocket({
  identity, pc, pair, href, origin = relayOrigin(href), WebSocketImpl = globalThis.WebSocket,
  onPairingAdmitted, onRefused, onClosed,
}) {
  const socket = new WebSocketImpl(signalingUrl(href, pc));
  let admitted = false;
  let challenged = false;
  const send = message => { try { socket.send(JSON.stringify(message)); } catch {} };
  socket.onopen = () => send(viewerHello({ identity, pair }));
  socket.onmessage = async event => {
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
      // TODO(dk2 handshake run): run the pairing handshake on this socket.
      onPairingAdmitted?.({ socket, message });
    }
  };
  socket.onclose = event => {
    if (!admitted && event.code === 1008) onRefused?.(event);
    else onClosed?.(event, { admitted });
  };
  return socket;
}
