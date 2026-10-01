const STUN_SERVER = { urls: "stun:stun.l.google.com:19302" };
const TURN_URLS = [
  "turn:turn.example.com:3478?transport=udp",
  "turn:turn.example.com:3478?transport=tcp",
];
const DEFAULT_TTL_SECONDS = 6 * 60 * 60;
const MIN_TTL_SECONDS = 5 * 60;
const MAX_TTL_SECONDS = 24 * 60 * 60;

// Coturn's TURN REST API uses an expiring username and the base64-encoded
// HMAC-SHA1(username, shared-secret) as the password. This only creates
// short-lived credentials; the shared secret never leaves the Worker.
// TURN is opt-in per client (`turn: true` in its hello): by default the
// browser is never told the TURN hostname, so it never contacts it.
export async function createTurnIceServers(env, { turn = false, now = Date.now } = {}) {
  if (turn !== true) return [STUN_SERVER];
  const secret = env?.TURN_SECRET;
  if (typeof secret !== "string" || !secret.trim()) return [STUN_SERVER];

  try {
    const expiry = Math.floor(now() / 1_000) + turnTtlSeconds(env.TURN_TTL_SECONDS);
    const username = `${expiry}:bubbleplay`;
    const key = await crypto.subtle.importKey(
      "raw",
      new TextEncoder().encode(secret),
      { name: "HMAC", hash: "SHA-1" },
      false,
      ["sign"],
    );
    const signature = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(username));
    return [
      STUN_SERVER,
      { urls: TURN_URLS, username, credential: base64(signature) },
    ];
  } catch {
    // A malformed or unavailable crypto environment must not block signaling.
    return [STUN_SERVER];
  }
}

function turnTtlSeconds(value) {
  const parsed = Number(value);
  if (!Number.isFinite(parsed)) return DEFAULT_TTL_SECONDS;
  return Math.min(MAX_TTL_SECONDS, Math.max(MIN_TTL_SECONDS, Math.floor(parsed)));
}

function base64(buffer) {
  let binary = "";
  for (const byte of new Uint8Array(buffer)) binary += String.fromCharCode(byte);
  return btoa(binary);
}
