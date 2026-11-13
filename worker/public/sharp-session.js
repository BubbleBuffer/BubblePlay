// The sharp-viewport conversation with the host (media-control viewport / media-view): which crop
// is wanted, which one is requested, which one the host committed, and what a stalled transition
// does. It owns that state; the receiver supplies the environment (desktop size, viewport
// geometry, transport) and renders the crop the session reports. Crop math lives in
// sharp-viewport.js.
import { cropGain, cropNeedsUpdate, desiredSharpCrop, fullDesktopCrop } from "./sharp-viewport.js";

// Requests are spaced at least this far apart; a change inside the window waits for it.
export const VIEWPORT_REQUEST_MIN_INTERVAL_MS = 100;
// A requested crop the host has not committed after this long first asks for a keyframe, then
// (after the same wait again) gives up and requests the full desktop.
export const VIEWPORT_TRANSITION_TIMEOUT_MS = 1_500;
// Consecutive failed transitions after which sharp zoom stays off for the session.
export const VIEWPORT_TIMEOUTS_BEFORE_DISABLE = 2;
// The wire carries crops as fractions of the desktop in unsigned 16 bits.
export const VIEWPORT_Q16_MAX = 65_535;
// Used for the gain figure until a decoded frame size is known.
export const VIEWPORT_DEFAULT_DECODED_WIDTH_PX = 1_920;
export const VIEWPORT_DEFAULT_DECODED_HEIGHT_PX = 1_080;
export const VIEWPORT_TIMEOUT_FULL_REASON = "viewport-timeout-full";
const HEALTH_DIGITS = 2;
const STATUS_GAIN_DIGITS = 1;

const cropLabel = crop => `${crop.width}x${crop.height}@${crop.x},${crop.y}`;
const q16 = (value, extent) => Math.max(0, Math.min(VIEWPORT_Q16_MAX, Math.round(value / Math.max(1, extent) * VIEWPORT_Q16_MAX)));

/**
 * env: {
 *   enabled: boolean, sharp zoom allowed on this page,
 *   desktop(): { width, height }, geometry(): { snapshot, content, innerWidth, innerHeight },
 *   streamMode(): "game" | "work", canSend(): boolean, send(fields), recover(reason),
 *   decoded(): { width, height } | null, log(level, event, detail), now(), setTimer(fn, ms), clearTimer(id)
 * }
 */
export function createSharpSession(env) {
  const now = env.now ?? (() => performance.now());
  const setTimer = env.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
  const clearTimer = env.clearTimer ?? (id => clearTimeout(id));
  let revision = 0;
  let timer = null;
  let transitionTimer = null;
  let last = null;
  let committed = { enabled: false, revision: 0, generation: 0 };
  let pending = null;
  let lastSentAt = -Infinity;
  let consecutiveTimeouts = 0;
  let disabledForSession = false;
  let telemetry = { state: "full", revision: 0, generation: 0, crop: `${VIEWPORT_DEFAULT_DECODED_WIDTH_PX}x${VIEWPORT_DEFAULT_DECODED_HEIGHT_PX}@0,0` };

  const fullCrop = () => fullDesktopCrop(env.desktop().width, env.desktop().height);
  const decoded = () => env.decoded() ?? { width: VIEWPORT_DEFAULT_DECODED_WIDTH_PX, height: VIEWPORT_DEFAULT_DECODED_HEIGHT_PX };

  function cropOf(message) {
    const desktop = env.desktop();
    const desktopWidth = Number(message?.desktopWidth) || desktop.width;
    const desktopHeight = Number(message?.desktopHeight) || desktop.height;
    const width = Number(message?.sourceWidth) || desktopWidth;
    const height = Number(message?.sourceHeight) || desktopHeight;
    return { enabled: message?.state !== "full" && (width < desktopWidth || height < desktopHeight),
      x: Number(message?.sourceX) || 0, y: Number(message?.sourceY) || 0, width, height,
      desktopWidth, desktopHeight, revision: Number(message?.revision) || 0,
      generation: Number(message?.generation) || 0, source: message };
  }

  function activeCrop() { return committed.source ? cropOf(committed.source) : fullCrop(); }

  function desiredCrop() {
    if (env.streamMode() === "game" || !env.enabled || disabledForSession) return fullCrop();
    const geometry = env.geometry();
    const desktop = env.desktop();
    return desiredSharpCrop({ viewport: geometry.snapshot, viewportWidth: geometry.innerWidth, viewportHeight: geometry.innerHeight,
      content: geometry.content, desktopWidth: desktop.width, desktopHeight: desktop.height,
      wasEnabled: committed.enabled || !!pending?.requested?.enabled });
  }

  function armTransitionTimeout(forRevision) {
    clearTimer(transitionTimer);
    transitionTimer = setTimer(() => {
      if (!pending || pending.revision !== forRevision) return;
      if (!pending.recoveryRequested) {
        pending.recoveryRequested = true;
        env.recover("viewport-transition-timeout");
        armTransitionTimeout(forRevision);
        return;
      }
      env.log("warn", "viewport-transition-failed", { revision: forRevision, requested: pending.requested });
      consecutiveTimeouts++;
      pending = null;
      if (consecutiveTimeouts >= VIEWPORT_TIMEOUTS_BEFORE_DISABLE) disabledForSession = true;
      request(fullCrop(), VIEWPORT_TIMEOUT_FULL_REASON, true);
    }, VIEWPORT_TRANSITION_TIMEOUT_MS);
  }

  function request(crop, reason, force = false) {
    // The crop travels out of band (media-control viewport / media-view); frames carry only a generation.
    if (!env.canSend() || pending) return false;
    const desktop = env.desktop();
    if (!force && !cropNeedsUpdate(activeCrop(), crop, desktop.width, desktop.height)) return false;
    revision = (revision + 1) >>> 0;
    lastSentAt = now();
    last = { enabled: crop.enabled, x: crop.x, y: crop.y, width: crop.width, height: crop.height };
    pending = { revision, requested: crop, reason, sentAt: lastSentAt, recoveryRequested: false, generation: 0, source: null };
    telemetry = { state: "refining", revision, generation: 0, crop: cropLabel(crop) };
    env.send({ revision, enabled: crop.enabled, xQ16: q16(crop.x, desktop.width), yQ16: q16(crop.y, desktop.height),
      widthQ16: q16(crop.width, desktop.width), heightQ16: q16(crop.height, desktop.height), reason });
    armTransitionTimeout(revision);
    return true;
  }

  function flush(reason) {
    timer = null;
    if (pending) return;
    request(desiredCrop(), reason);
  }

  return {
    /** Asks for the crop the viewport now wants, at most once per VIEWPORT_REQUEST_MIN_INTERVAL_MS. */
    schedule(reason = "sharp-viewport") {
      if (!env.canSend() || pending) return;
      clearTimer(timer);
      const wait = Math.max(0, VIEWPORT_REQUEST_MIN_INTERVAL_MS - (now() - lastSentAt));
      timer = setTimer(() => flush(reason), wait);
    },
    /** A media-view message from the host: a pending acknowledgement or a committed crop. */
    onMediaView(message) {
      if (!Number.isInteger(message.revision)) return;
      if (message.revision < Math.max(committed.revision || 0, pending?.revision || 0)) return;
      telemetry = { state: message.state || "?", revision: message.revision, generation: message.generation || 0,
        crop: `${message.sourceWidth || "?"}x${message.sourceHeight || "?"}@${message.sourceX || 0},${message.sourceY || 0}` };
      if (message.state === "pending") {
        if (pending?.revision === message.revision) Object.assign(pending, { source: message, generation: message.generation || 0 });
        return;
      }
      const crop = cropOf(message);
      const completed = pending?.revision === message.revision ? pending : null;
      committed = { enabled: crop.enabled, revision: message.revision, generation: message.generation || 0, source: message };
      if (completed) {
        pending = null;
        if (crop.enabled || completed.reason !== VIEWPORT_TIMEOUT_FULL_REASON) consecutiveTimeouts = 0;
        clearTimer(transitionTimer); transitionTimer = null;
      }
      const gain = cropGain(crop, decoded().width, decoded().height);
      telemetry = { state: crop.enabled ? `sharp ${gain.x.toFixed(STATUS_GAIN_DIGITS)}x/${gain.y.toFixed(STATUS_GAIN_DIGITS)}x` : "full",
        revision: message.revision, generation: message.generation || 0, crop: cropLabel(crop) };
      this.schedule("viewport-committed");
    },
    /** The crop a frame of `generation` must be drawn with, or null: never render a generation with stale geometry. */
    cropForGeneration(generation) {
      if (pending?.source && pending.generation === generation) return cropOf(pending.source);
      if (committed.source && committed.generation === generation) return cropOf(committed.source);
      if (!pending && committed.source) return { ...cropOf(committed.source), generation };
      if (!pending && !committed.enabled) return { ...fullCrop(), generation };
      return null;
    },
    activeCrop,
    /** True while a requested crop is not yet committed. */
    get isPending() { return pending !== null; },
    /** The committed crop's `enabled` and `revision` (file drop checks them against the rendered overlay). */
    get committed() { return { enabled: committed.enabled, revision: committed.revision }; },
    /** { state, revision, crop } for the HUD. */
    get telemetry() { return { state: telemetry.state, revision: telemetry.revision, crop: telemetry.crop }; },
    health(renderedView, zoom, pointerMode) {
      const crop = renderedView || activeCrop();
      const gain = cropGain(crop, decoded().width, decoded().height);
      return { state: telemetry.state, revision: telemetry.revision, generation: telemetry.generation,
        crop: telemetry.crop, zoom: Number(zoom.toFixed(HEALTH_DIGITS)), pointer: pointerMode,
        pending: pending?.revision || null,
        transitionMs: pending ? Math.round(now() - pending.sentAt) : 0,
        gainX: Number(gain.x.toFixed(HEALTH_DIGITS)), gainY: Number(gain.y.toFixed(HEALTH_DIGITS)), requested: last };
    },
    /** Drops all negotiation state (a new media start); the revision counter keeps counting. */
    reset() {
      committed = { enabled: false, revision, generation: 0 };
      pending = null;
      last = null;
      clearTimer(timer); timer = null;
      clearTimer(transitionTimer); transitionTimer = null;
      consecutiveTimeouts = 0;
      disabledForSession = false;
    },
  };
}
