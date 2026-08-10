export const STUN_FALLBACK = Object.freeze([Object.freeze({ urls: "stun:stun.l.google.com:19302" })]);

const MAX_SERVERS = 4;
const MAX_URLS_PER_SERVER = 4;
const MAX_URL_LENGTH = 256;
const MAX_CREDENTIAL_LENGTH = 512;
const SAFE_CANDIDATE_TYPES = new Set(["host", "srflx", "prflx", "relay"]);

// Treat the signaling payload as untrusted: an invalid member rejects the
// entire configuration, leaving the direct STUN route available.
export function validateIceServers(value) {
  if (!Array.isArray(value) || value.length === 0 || value.length > MAX_SERVERS) return STUN_FALLBACK;
  const servers = [];
  for (const server of value) {
    if (!server || typeof server !== "object" || Array.isArray(server)) return STUN_FALLBACK;
    const urls = normalizeUrls(server.urls);
    if (!urls) return STUN_FALLBACK;
    const hasTurn = urls.some((url) => /^(turn|turns):/i.test(url));
    const username = server.username;
    const credential = server.credential;
    if (hasTurn && (!isBoundedNonemptyString(username) || !isBoundedNonemptyString(credential))) return STUN_FALLBACK;
    if ((!hasTurn && (username !== undefined || credential !== undefined)) ||
        (username !== undefined && !isBoundedNonemptyString(username)) ||
        (credential !== undefined && !isBoundedNonemptyString(credential))) return STUN_FALLBACK;
    const normalized = { urls: typeof server.urls === "string" ? urls[0] : urls };
    if (hasTurn) {
      normalized.username = username;
      normalized.credential = credential;
    }
    servers.push(normalized);
  }
  return servers;
}

export function parseIceTransportPolicy(search) {
  const params = typeof search === "string" ? new URLSearchParams(search) : search;
  return params?.get?.("ice") === "relay" ? "relay" : "all";
}

export function deriveCandidateRoute(profile) {
  const local = profile?.candidatePair?.local?.candidateType;
  const remote = profile?.candidatePair?.remote?.candidateType;
  return SAFE_CANDIDATE_TYPES.has(local) && SAFE_CANDIDATE_TYPES.has(remote) ? `${local}->${remote}` : "…";
}

function normalizeUrls(value) {
  const urls = typeof value === "string" ? [value] : value;
  if (!Array.isArray(urls) || urls.length === 0 || urls.length > MAX_URLS_PER_SERVER) return null;
  for (const url of urls) {
    if (!isValidIceUrl(url)) return null;
  }
  return urls.slice();
}

function isValidIceUrl(url) {
  if (typeof url !== "string" || url.length === 0 || url.length > MAX_URL_LENGTH || /[\s\x00-\x1f\x7f]/.test(url)) return false;
  const match = /^(stun|turn|turns):([^?#]+)(?:\?([^#]*))?$/i.exec(url);
  if (!match) return false;
  const [, scheme, authority, query] = match;
  if (!isValidIceAuthority(authority)) return false;
  if (scheme.toLowerCase() === "stun") return query === undefined;
  return query === undefined || /^(transport=udp|transport=tcp)$/i.test(query);
}

function isValidIceAuthority(authority) {
  if (authority.includes("@") || authority.includes("/")) return false;
  if (authority.startsWith("[")) return /^\[[0-9a-f:.%]+\](?::\d{1,5})?$/i.test(authority);
  const match = /^([^:]+)(?::(\d{1,5}))?$/.exec(authority);
  if (!match || !/^[a-z0-9][a-z0-9.-]*$/i.test(match[1])) return false;
  return !match[2] || Number(match[2]) <= 65535;
}

function isBoundedNonemptyString(value) {
  return typeof value === "string" && value.length > 0 && value.length <= MAX_CREDENTIAL_LENGTH;
}
