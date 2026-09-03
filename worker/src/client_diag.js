const DIAGNOSTIC_PREFIX = "client-diag:";
const DIAGNOSTIC_INDEX = `${DIAGNOSTIC_PREFIX}index`;
const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const STAGE_PATTERN = /^[a-z][a-z0-9-]{0,63}$/;
const DETAIL_KEY_PATTERN = /^[A-Za-z][A-Za-z0-9]{0,31}$/;
const MAX_BODY_BYTES = 4_096;
const MAX_EVENTS = 128;
const MAX_ATTEMPTS = 8;
const ATTEMPT_TTL_MS = 30 * 60 * 1_000;
const EVENT_FIELDS = new Set([
  "schemaVersion", "attemptId", "runId", "compatibilityId", "receiverBuild",
  "seq", "atMs", "stage", "detail",
]);

export const CLIENT_DIAGNOSTIC_PATH = "/__diag/client";

export function clientDiagnosticAttempt(pathname) {
  const match = pathname.match(/^\/__diag\/client\/([0-9a-f-]+)$/i);
  return match && UUID_PATTERN.test(match[1]) ? match[1].toLowerCase() : null;
}

export async function ingestClientDiagnostic(storage, raw, compatibility, now = Date.now()) {
  if (typeof raw !== "string" || raw.length === 0 || new TextEncoder().encode(raw).byteLength > MAX_BODY_BYTES)
    return { ok: false, status: 413, reason: "body-size" };
  let value;
  try { value = JSON.parse(raw); } catch { return { ok: false, status: 400, reason: "json" }; }
  const event = validateEvent(value, compatibility);
  if (!event) return { ok: false, status: 400, reason: "shape" };

  const index = await pruneIndex(storage, now);
  const key = `${DIAGNOSTIC_PREFIX}${event.attemptId}`;
  const existing = await storage.get(key);
  const timeline = existing && existing.expiresAt > now
    ? existing
    : { attemptId: event.attemptId, createdAt: now, lastAt: now, expiresAt: now + ATTEMPT_TTL_MS, events: [] };
  timeline.lastAt = now;
  timeline.expiresAt = now + ATTEMPT_TTL_MS;
  timeline.events = timeline.events.filter(item => item.seq !== event.seq);
  timeline.events.push({ ...event, serverAtMs: now });
  timeline.events.sort((left, right) => left.seq - right.seq || left.serverAtMs - right.serverAtMs);
  if (timeline.events.length > MAX_EVENTS) timeline.events.splice(0, timeline.events.length - MAX_EVENTS);
  await storage.put(key, timeline);

  const nextIndex = index.filter(item => item.attemptId !== event.attemptId);
  nextIndex.push({ attemptId: event.attemptId, lastAt: now, expiresAt: timeline.expiresAt });
  nextIndex.sort((left, right) => left.lastAt - right.lastAt);
  while (nextIndex.length > MAX_ATTEMPTS) {
    const removed = nextIndex.shift();
    await storage.delete(`${DIAGNOSTIC_PREFIX}${removed.attemptId}`);
  }
  await storage.put(DIAGNOSTIC_INDEX, nextIndex);
  return { ok: true, status: 204, event: { ...event, serverAtMs: now } };
}

export async function readClientDiagnostic(storage, attemptId, now = Date.now()) {
  if (!UUID_PATTERN.test(attemptId || "")) return null;
  const key = `${DIAGNOSTIC_PREFIX}${attemptId.toLowerCase()}`;
  const timeline = await storage.get(key);
  if (!timeline || timeline.expiresAt <= now) {
    if (timeline) await storage.delete(key);
    return null;
  }
  return timeline;
}

async function pruneIndex(storage, now) {
  const stored = await storage.get(DIAGNOSTIC_INDEX);
  const index = Array.isArray(stored) ? stored : [];
  const live = [];
  for (const item of index) {
    if (!item || !UUID_PATTERN.test(item.attemptId || "") || !Number.isFinite(item.expiresAt) || item.expiresAt <= now) {
      if (item?.attemptId && UUID_PATTERN.test(item.attemptId))
        await storage.delete(`${DIAGNOSTIC_PREFIX}${item.attemptId.toLowerCase()}`);
      continue;
    }
    live.push(item);
  }
  return live;
}

function validateEvent(value, compatibility) {
  if (!plainObject(value) || Object.keys(value).some(key => !EVENT_FIELDS.has(key))) return null;
  if (value.schemaVersion !== 1 || !UUID_PATTERN.test(value.attemptId || "") ||
      !Number.isSafeInteger(value.seq) || value.seq < 1 || value.seq > 1_000_000 ||
      !Number.isSafeInteger(value.atMs) || value.atMs < 0 || !STAGE_PATTERN.test(value.stage || "")) return null;
  const runId = value.runId === null ? null : String(value.runId || "");
  if (runId !== null && !UUID_PATTERN.test(runId)) return null;
  const compatibilityId = diagnosticLabel(value.compatibilityId);
  if (compatibilityId === undefined) return null;
  const receiverBuild = diagnosticLabel(value.receiverBuild);
  if (receiverBuild === undefined) return null;
  const detail = sanitizeDetail(value.detail);
  if (detail === null) return null;
  return {
    schemaVersion: 1,
    attemptId: value.attemptId.toLowerCase(),
    runId: runId?.toLowerCase() ?? null,
    compatibilityId,
    compatibilityMatch: compatibilityId === compatibility.id,
    receiverBuild,
    receiverBuildMatch: receiverBuild === compatibility.receiverBuild,
    seq: value.seq,
    atMs: value.atMs,
    stage: value.stage,
    detail,
  };
}

function diagnosticLabel(value) {
  if (value === null) return null;
  if (typeof value !== "string" || value.length === 0 || value.length > 128 || /[\u0000-\u001f\u007f]/.test(value))
    return undefined;
  return value;
}

function sanitizeDetail(value) {
  if (!plainObject(value) || Object.keys(value).length > 16) return null;
  const result = {};
  for (const [key, item] of Object.entries(value)) {
    if (!DETAIL_KEY_PATTERN.test(key)) return null;
    const clean = sanitizeDetailValue(item);
    if (clean === undefined) return null;
    result[key] = clean;
  }
  return result;
}

function sanitizeDetailValue(value) {
  if (value === null || typeof value === "boolean") return value;
  if (typeof value === "number") return Number.isFinite(value) ? value : undefined;
  if (typeof value === "string") return value.length <= 512 ? value : value.slice(0, 512);
  if (Array.isArray(value) && value.length <= 16) {
    const result = value.map(sanitizeDetailValue);
    return result.some(item => item === undefined || Array.isArray(item)) ? undefined : result;
  }
  return undefined;
}

function plainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value) &&
    (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);
}
