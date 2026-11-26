// Wire side of window-list-v1 (docs/architecture/app-focus-mode.md,
// fixtures/window-list-contract.json): names, limits, parsers for the host's
// messages and builders for the viewer's. Pure; the controller is app-focus.js.

export const WINDOW_LIST_CAP = "window-list-v1";
export const WINDOW_LIST_VERSION = 1;
export const TYPES = Object.freeze({
  list: "window-list", focus: "window-focus", failed: "window-focus-failed",
  icon: "window-icon", start: "window-list-start", stop: "window-list-stop",
});
// Host to viewer; routed to the window-list handler only when both sides negotiated the cap.
export const HOST_TYPES = new Set([TYPES.list, TYPES.failed, TYPES.icon]);
export const LIMITS = Object.freeze({
  maxWindows: 64, maxTitleBytes: 256, maxProcessBytes: 128, iconSize: 32, maxIconBytes: 8192,
  minSendIntervalMs: 250, maxListMessageBytes: 65536,
});
export const STATES = Object.freeze({ foreground: "foreground", normal: "normal", minimized: "minimized" });
export const FAILURES = Object.freeze({ gone: "gone", elevated: "elevated", refused: "refused" });

const MAX_SELECTOR = 128;
const RECT_FIELDS = 4; // [left, top, width, height]
const U32_MAX = 0xffff_ffff;
const ICON_HASH = /^[0-9a-f]{16}$/;
const BASE64 = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/;
const PNG_SIGNATURE = "\x89PNG\r\n\x1a\n";

const isId = value => Number.isSafeInteger(value) && value >= 0 && value <= U32_MAX;
export const isIconHash = value => typeof value === "string" && ICON_HASH.test(value);
const isOwn = (message, type) => Boolean(message) && typeof message === "object" && message.type === type && message.v === WINDOW_LIST_VERSION;

function parseEntry(raw) {
  if (!raw || typeof raw !== "object" || !isId(raw.id)) return null;
  if (typeof raw.title !== "string" || raw.title.length > LIMITS.maxTitleBytes) return null;
  if (typeof raw.process !== "string" || raw.process.length > LIMITS.maxProcessBytes) return null;
  if (!Object.hasOwn(STATES, raw.state)) return null;
  const rect = raw.rect;
  if (!Array.isArray(rect) || rect.length !== RECT_FIELDS || !rect.every(Number.isSafeInteger) || rect[2] <= 0 || rect[3] <= 0) return null;
  if (raw.monitor !== undefined && (typeof raw.monitor !== "string" || !raw.monitor || raw.monitor.length > MAX_SELECTOR)) return null;
  if (raw.icon !== undefined && !isIconHash(raw.icon)) return null;
  return { id: raw.id, title: raw.title, process: raw.process, state: raw.state,
    rect: { left: rect[0], top: rect[1], width: rect[2], height: rect[3] },
    monitor: raw.monitor ?? null, icon: raw.icon ?? null };
}

/** The valid entries of a window-list message in order (invalid ones dropped), or null for another message. */
export function parseWindowList(message) {
  if (!isOwn(message, TYPES.list) || !Array.isArray(message.windows)) return null;
  const windows = [];
  for (const raw of message.windows.slice(0, LIMITS.maxWindows)) {
    const entry = parseEntry(raw);
    if (entry) windows.push(entry);
  }
  return windows;
}

export function parseFocusFailed(message) {
  if (!isOwn(message, TYPES.failed) || !isId(message.id) || !Object.hasOwn(FAILURES, message.reason)) return null;
  return { id: message.id, reason: message.reason };
}

/** { hash, bytes } of a valid icon answer, else null. */
export function parseIcon(message) {
  if (!isOwn(message, TYPES.icon) || !isIconHash(message.icon) || typeof message.png !== "string" || !BASE64.test(message.png)) return null;
  let binary;
  try { binary = atob(message.png); } catch { return null; }
  if (binary.length > LIMITS.maxIconBytes || !binary.startsWith(PNG_SIGNATURE)) return null;
  return { hash: message.icon, bytes: Uint8Array.from(binary, ch => ch.charCodeAt(0)) };
}

const base = type => ({ type, v: WINDOW_LIST_VERSION });
export const startMessage = () => base(TYPES.start);
export const stopMessage = () => base(TYPES.stop);
/** Null for an id the host would reject. */
export const focusMessage = id => isId(id) ? { ...base(TYPES.focus), id } : null;
export const iconRequest = hash => isIconHash(hash) ? { ...base(TYPES.icon), icon: hash } : null;
