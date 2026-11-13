// Client side of monitor-previews-v1 (docs/architecture/monitor-previews.md,
// fixtures/monitor-previews-contract.json). The host sends a small JPEG of each
// monitor it is not streaming; on a monitor switch the viewer shows the target's
// picture at once and fades it out on the first live frame. Pictures are held in
// memory only, one per monitor selector, and never block video or input.

export const PREVIEW_CAP = "monitor-previews-v1";
export const PREVIEW_TYPE = "monitor-preview";
export const PREVIEW_LIMITS = Object.freeze({ maxJpegBytes: 98304, maxMessageJsonBytes: 140000, maxDimension: 960 });
export const FADE_MS = 200;
// A picture that never gets its first live frame must not stay on screen forever.
export const SAFETY_MS = 5000;

const BASE64 = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/;

function decodeBase64(text) {
  if (typeof text !== "string" || !text || text.length > PREVIEW_LIMITS.maxMessageJsonBytes || !BASE64.test(text)) return null;
  try {
    const binary = atob(text);
    const out = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
    return out;
  } catch { return null; }
}

/** Returns { monitor, seq, w, h, bytes } for an acceptable message, otherwise null (ignore it). */
export function parseMonitorPreview(message) {
  if (!message || typeof message !== "object" || message.type !== PREVIEW_TYPE || message.v !== 1) return null;
  if (typeof message.monitor !== "string" || !message.monitor || message.monitor.length > 128) return null;
  if (!Number.isSafeInteger(message.seq) || message.seq < 0) return null;
  const max = PREVIEW_LIMITS.maxDimension;
  if (!Number.isInteger(message.w) || !Number.isInteger(message.h) || message.w < 1 || message.h < 1 || message.w > max || message.h > max) return null;
  const bytes = decodeBase64(message.jpeg);
  if (!bytes || bytes.length > PREVIEW_LIMITS.maxJpegBytes) return null;
  if (bytes.length < 3 || bytes[0] !== 0xff || bytes[1] !== 0xd8 || bytes[2] !== 0xff) return null;
  return { monitor: message.monitor, seq: message.seq, w: message.w, h: message.h, bytes };
}

/** Latest picture per monitor selector, the seq rule, and the live monitor. */
export function createPreviewStore() {
  const held = new Map();      // selector -> picture
  const lastSeq = new Map();   // selector -> highest seq seen this session
  let live = null;
  return {
    /** Hold a validated picture. False when ignored (older seq, or the monitor is live). */
    hold(picture) {
      if (!picture || picture.monitor === live) return false;
      const last = lastSeq.get(picture.monitor);
      if (last !== undefined && picture.seq < last) return false;
      lastSeq.set(picture.monitor, picture.seq);
      held.set(picture.monitor, picture);
      return true;
    },
    /** The selected monitor of a monitor-list. Returns the picture to show when the selection changed, else null. */
    select(selector) {
      const previous = live;
      live = typeof selector === "string" && selector ? selector : null;
      if (live === null || live === previous) return null;
      const picture = held.get(live) || null;
      held.delete(live);
      // The first list of a session only establishes which monitor is live.
      return previous === null ? null : picture;
    },
    get(selector) { return held.get(selector) || null; },
    get size() { return held.size; },
    get live() { return live; },
    clear() { held.clear(); lastSeq.clear(); live = null; },
  };
}

/**
 * Overlay controller. `el` is an <img>-like element (src, hidden, style); the URL
 * and timer functions are seams for tests. show() is immediate; fadeOut() is
 * called on the first live frame.
 */
export function createPreviewOverlay({ el, createUrl, revokeUrl, schedule = setTimeout, cancel = clearTimeout,
  fadeMs = FADE_MS, safetyMs = SAFETY_MS, makeBlob = bytes => new Blob([bytes], { type: "image/jpeg" }) }) {
  let url = null, timer = null, safety = null, visible = false;
  const release = () => {
    cancel(timer); timer = null;
    cancel(safety); safety = null;
    visible = false;
    if (el) { el.hidden = true; el.style.opacity = "0"; try { el.removeAttribute?.("src"); } catch { /* an element without removeAttribute keeps its src */ } }
    if (url) { try { revokeUrl(url); } catch { /* URL already revoked */ } url = null; }
  };
  const fadeOut = () => {
    if (!visible || timer) return;
    cancel(safety); safety = null;
    el.style.transition = `opacity ${fadeMs}ms linear`;
    el.style.opacity = "0";
    timer = schedule(release, fadeMs + 50);
  };
  return {
    get visible() { return visible; },
    show(picture) {
      if (!el || !picture) return false;
      release();
      try { url = createUrl(makeBlob(picture.bytes)); } catch { url = null; return false; }
      el.style.transition = "none";
      el.style.opacity = "1";
      el.src = url;
      el.hidden = false;
      visible = true;
      safety = schedule(fadeOut, safetyMs);
      return true;
    },
    fadeOut,
    hide: release,
  };
}

/** The receiver's whole interface: onMessage, onMonitorList, onFirstFrame, reset. */
export function createMonitorPreviews({ el, createUrl = blob => URL.createObjectURL(blob), revokeUrl = url => URL.revokeObjectURL(url), ...rest }) {
  const store = createPreviewStore();
  const overlay = createPreviewOverlay({ el, createUrl, revokeUrl, ...rest });
  return {
    store, overlay,
    onMessage(message) {
      const picture = parseMonitorPreview(message);
      return picture ? store.hold(picture) : false;
    },
    onMonitorList(monitors) {
      const selected = Array.isArray(monitors) ? monitors.find(m => m && m.selected) : null;
      const picture = store.select(selected ? selected.selector : null);
      if (picture) overlay.show(picture);
      return Boolean(picture);
    },
    onFirstFrame() { overlay.fadeOut(); },
    reset() { overlay.hide(); store.clear(); },
  };
}
