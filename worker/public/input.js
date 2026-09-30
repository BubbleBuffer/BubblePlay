export const packet = {
  key: (vk, up) => Uint8Array.of(up ? 1 : 0, vk & 255, vk >> 8), buttons: mask => Uint8Array.of(4, mask), releaseAll: () => Uint8Array.of(6),
  text: value => { const bytes = new TextEncoder().encode(String(value)); if (!bytes.length || bytes.length > 512) return null; const out = new Uint8Array(bytes.length + 1); out[0] = 11; out.set(bytes, 1); return out; },
  motion: (seq, dx, dy, wheelX = 0, wheelY = 0) => { const d = new Uint8Array(11), v = new DataView(d.buffer); v.setUint8(0, 7); v.setUint16(1, seq, true); v.setInt16(3, dx, true); v.setInt16(5, dy, true); v.setInt16(7, wheelX, true); v.setInt16(9, wheelY, true); return d; },
  absolute: (seq, x, y) => { const d = new Uint8Array(7), v = new DataView(d.buffer); v.setUint8(0, 8); v.setUint16(1, seq, true); v.setUint16(3, x, true); v.setUint16(5, y, true); return d; },
  buttonsAt: (seq, x, y, mask) => { const d = new Uint8Array(8), v = new DataView(d.buffer); v.setUint8(0, 9); v.setUint16(1, seq, true); v.setUint16(3, x, true); v.setUint16(5, y, true); v.setUint8(7, mask); return d; },
  cursorSync: seq => Uint8Array.of(10, seq & 255, seq >> 8),
  // Physical key: set-1 scan code (0 = use the VK fallback), E0-extended flag.
  scanKey: (scan, extended, vk, up) => Uint8Array.of(13, (up ? 1 : 0) | (extended ? 2 : 0), scan & 255, scan >> 8, vk & 255, vk >> 8),
  clipboardChunk: (id, index, count, bytes, paste) => { const out = new Uint8Array(8 + bytes.length), v = new DataView(out.buffer); v.setUint8(0, 12); v.setUint8(1, paste ? 1 : 0); v.setUint16(2, id, true); v.setUint16(4, index, true); v.setUint16(6, count, true); out.set(bytes, 8); return out; },
};

// ---- keyboard: KeyboardEvent.code -> Windows set-1 scan code -----------------
// [scan, extended(E0)] per physical key, matching Chromium's Windows scancode
// table. The host injects KEYEVENTF_SCANCODE so its own keyboard layout maps
// the key (AltGr, AZERTY/QWERTZ, dead keys). Kept in sync with
// fixtures/input-contract.json by worker/test/input.test.mjs.
const SCAN_TABLE = [
  ["Escape", 0x01], ["Digit1", 0x02], ["Digit2", 0x03], ["Digit3", 0x04], ["Digit4", 0x05], ["Digit5", 0x06],
  ["Digit6", 0x07], ["Digit7", 0x08], ["Digit8", 0x09], ["Digit9", 0x0a], ["Digit0", 0x0b], ["Minus", 0x0c],
  ["Equal", 0x0d], ["Backspace", 0x0e], ["Tab", 0x0f], ["KeyQ", 0x10], ["KeyW", 0x11], ["KeyE", 0x12],
  ["KeyR", 0x13], ["KeyT", 0x14], ["KeyY", 0x15], ["KeyU", 0x16], ["KeyI", 0x17], ["KeyO", 0x18], ["KeyP", 0x19],
  ["BracketLeft", 0x1a], ["BracketRight", 0x1b], ["Enter", 0x1c], ["ControlLeft", 0x1d], ["KeyA", 0x1e],
  ["KeyS", 0x1f], ["KeyD", 0x20], ["KeyF", 0x21], ["KeyG", 0x22], ["KeyH", 0x23], ["KeyJ", 0x24], ["KeyK", 0x25],
  ["KeyL", 0x26], ["Semicolon", 0x27], ["Quote", 0x28], ["Backquote", 0x29], ["ShiftLeft", 0x2a],
  ["Backslash", 0x2b], ["KeyZ", 0x2c], ["KeyX", 0x2d], ["KeyC", 0x2e], ["KeyV", 0x2f], ["KeyB", 0x30],
  ["KeyN", 0x31], ["KeyM", 0x32], ["Comma", 0x33], ["Period", 0x34], ["Slash", 0x35], ["ShiftRight", 0x36],
  ["NumpadMultiply", 0x37], ["AltLeft", 0x38], ["Space", 0x39], ["CapsLock", 0x3a], ["F1", 0x3b], ["F2", 0x3c],
  ["F3", 0x3d], ["F4", 0x3e], ["F5", 0x3f], ["F6", 0x40], ["F7", 0x41], ["F8", 0x42], ["F9", 0x43], ["F10", 0x44],
  ["ScrollLock", 0x46], ["Numpad7", 0x47], ["Numpad8", 0x48], ["Numpad9", 0x49], ["NumpadSubtract", 0x4a],
  ["Numpad4", 0x4b], ["Numpad5", 0x4c], ["Numpad6", 0x4d], ["NumpadAdd", 0x4e], ["Numpad1", 0x4f],
  ["Numpad2", 0x50], ["Numpad3", 0x51], ["Numpad0", 0x52], ["NumpadDecimal", 0x53], ["IntlBackslash", 0x56],
  ["F11", 0x57], ["F12", 0x58], ["NumpadEqual", 0x59], ["F13", 0x64], ["F14", 0x65], ["F15", 0x66], ["F16", 0x67],
  ["F17", 0x68], ["F18", 0x69], ["F19", 0x6a], ["F20", 0x6b], ["F21", 0x6c], ["F22", 0x6d], ["F23", 0x6e],
  ["KanaMode", 0x70], ["Lang2", 0x71], ["Lang1", 0x72], ["IntlRo", 0x73], ["F24", 0x76], ["Convert", 0x79],
  ["NonConvert", 0x7b], ["IntlYen", 0x7d], ["NumpadComma", 0x7e],
  // E0-prefixed (KEYEVENTF_EXTENDEDKEY) keys.
  ["MediaTrackPrevious", 0x10, true], ["MediaTrackNext", 0x19, true], ["NumpadEnter", 0x1c, true],
  ["ControlRight", 0x1d, true], ["AudioVolumeMute", 0x20, true], ["LaunchApp2", 0x21, true],
  ["MediaPlayPause", 0x22, true], ["MediaStop", 0x24, true], ["AudioVolumeDown", 0x2e, true],
  ["AudioVolumeUp", 0x30, true], ["BrowserHome", 0x32, true], ["NumpadDivide", 0x35, true],
  ["PrintScreen", 0x37, true], ["AltRight", 0x38, true], ["Home", 0x47, true], ["ArrowUp", 0x48, true],
  ["PageUp", 0x49, true], ["ArrowLeft", 0x4b, true], ["ArrowRight", 0x4d, true], ["End", 0x4f, true],
  ["ArrowDown", 0x50, true], ["PageDown", 0x51, true], ["Insert", 0x52, true], ["Delete", 0x53, true],
  ["MetaLeft", 0x5b, true], ["MetaRight", 0x5c, true], ["ContextMenu", 0x5d, true],
  ["BrowserSearch", 0x65, true], ["BrowserFavorites", 0x66, true], ["BrowserRefresh", 0x67, true],
  ["BrowserStop", 0x68, true], ["BrowserForward", 0x69, true], ["BrowserBack", 0x6a, true],
  ["LaunchApp1", 0x6b, true], ["LaunchMail", 0x6c, true], ["MediaSelect", 0x6d, true],
];
export const SCAN_CODES = Object.freeze(Object.fromEntries(SCAN_TABLE.map(([code, scan, extended = false]) => [code, Object.freeze([scan, extended])])));

// Windows VK per physical key. Legacy hosts (no "scancode" input feature)
// receive only this; scan-code hosts use it only when no scan code exists
// (Pause and NumLock have no single-byte set-1 make code SendInput accepts).
export const VK_CODES = (() => {
  const vk = {
    Backspace: 0x08, Tab: 0x09, Enter: 0x0D, NumpadEnter: 0x0D, ShiftLeft: 0xA0, ShiftRight: 0xA1,
    ControlLeft: 0xA2, ControlRight: 0xA3, AltLeft: 0xA4, AltRight: 0xA5,
    CapsLock: 0x14, Escape: 0x1B, Space: 0x20, PageUp: 0x21, PageDown: 0x22,
    End: 0x23, Home: 0x24, ArrowLeft: 0x25, ArrowUp: 0x26, ArrowRight: 0x27,
    ArrowDown: 0x28, PrintScreen: 0x2C, Insert: 0x2D, Delete: 0x2E,
    MetaLeft: 0x5B, MetaRight: 0x5C, ContextMenu: 0x5D,
    NumpadMultiply: 0x6A, NumpadAdd: 0x6B, NumpadSubtract: 0x6D, NumpadDecimal: 0x6E, NumpadDivide: 0x6F,
    Semicolon: 0xBA, Equal: 0xBB, Comma: 0xBC, Minus: 0xBD, Period: 0xBE,
    Slash: 0xBF, Backquote: 0xC0, BracketLeft: 0xDB, Backslash: 0xDC,
    BracketRight: 0xDD, Quote: 0xDE, IntlBackslash: 0xE2,
    NumLock: 0x90, ScrollLock: 0x91, Pause: 0x13,
  };
  for (let i = 0; i < 26; i++) vk["Key" + String.fromCharCode(65 + i)] = 0x41 + i;
  for (let i = 0; i < 10; i++) { vk["Digit" + i] = 0x30 + i; vk["Numpad" + i] = 0x60 + i; }
  for (let i = 1; i <= 24; i++) vk["F" + i] = 0x70 + (i - 1);
  return Object.freeze(vk);
})();
// Legacy VK packets historically sent the generic modifier VKs; keep that
// exact behaviour for hosts that predate scan-code input.
const LEGACY_VK = { ShiftLeft: 0x10, ShiftRight: 0x10, ControlLeft: 0x11, ControlRight: 0x11, AltLeft: 0x12, AltRight: 0x12 };

/** Returns { scan, extended, vk, legacyVk } for a KeyboardEvent.code, or null. */
export function keyDescriptor(code) {
  const scan = SCAN_CODES[code];
  const vk = VK_CODES[code];
  if (!scan && vk === undefined) return null;
  return { scan: scan ? scan[0] : 0, extended: scan ? scan[1] : false, vk: vk ?? 0, legacyVk: LEGACY_VK[code] ?? vk };
}

/**
 * Accumulates high-resolution wheel deltas in WHEEL_DELTA units (120 per
 * notch) and emits only whole units, carrying the fractional remainder so
 * trackpad/smooth scrolling never loses distance.
 */
export class WheelAccumulator {
  constructor({ pixelsPerNotch = 100, linesPerNotch = 3, pagesPerNotch = 1 / 3 } = {}) { Object.assign(this, { pixelsPerNotch, linesPerNotch, pagesPerNotch }); this.reset(); }
  reset() { this.x = 0; this.y = 0; }
  toUnits(delta, mode) {
    const perNotch = mode === 1 ? this.linesPerNotch : mode === 2 ? this.pagesPerNotch : this.pixelsPerNotch;
    return Number.isFinite(delta) ? delta * 120 / perNotch : 0;
  }
  /** Browser deltaY is positive downward; Windows wheel is positive upward. */
  add(deltaX, deltaY, deltaMode = 0) {
    this.x += this.toUnits(deltaX, deltaMode);
    this.y -= this.toUnits(deltaY, deltaMode);
    const x = Math.trunc(this.x), y = Math.trunc(this.y);
    this.x -= x; this.y -= y;
    return { x: clamp(x, -32768, 32767), y: clamp(y, -32768, 32767) };
  }
}

// ---- clipboard (text, both directions) --------------------------------------
export const CLIPBOARD = Object.freeze({ maxBytes: 1024 * 1024, chunkBytes: 8 * 1024, maxChunks: 128, hostMaxChunks: 64 });

/** Splits UTF-8 clipboard text into binary type-12 packets, or null if unusable. */
export function clipboardPackets(text, id, { paste = true } = {}) {
  const bytes = new TextEncoder().encode(String(text ?? ""));
  if (!bytes.length || bytes.length > CLIPBOARD.maxBytes) return null;
  const count = Math.ceil(bytes.length / CLIPBOARD.chunkBytes);
  const packets = [];
  for (let index = 0; index < count; index++) {
    packets.push(packet.clipboardChunk(id & 0xffff, index, count,
      bytes.subarray(index * CLIPBOARD.chunkBytes, (index + 1) * CLIPBOARD.chunkBytes), paste));
  }
  return packets;
}

/** Reassembles host `clipboard` JSON chunks; returns the text when complete. */
export class ClipboardReceiver {
  constructor() { this.reset(); }
  reset() { this.id = null; this.next = 0; this.count = 0; this.parts = []; this.bytes = 0; }
  push(message) {
    const { id, index, count, text } = message || {};
    if (message?.protocol !== 1 || !Number.isInteger(id) || !Number.isInteger(index) || !Number.isInteger(count) ||
        typeof text !== "string" || count < 1 || count > CLIPBOARD.hostMaxChunks || index < 0 || index >= count) { this.reset(); return null; }
    if (index === 0) { this.reset(); this.id = id; this.count = count; }
    else if (id !== this.id || count !== this.count || index !== this.next) { this.reset(); return null; }
    this.parts.push(text); this.next = index + 1; this.bytes += text.length;
    if (this.bytes > CLIPBOARD.maxBytes) { this.reset(); return null; }
    if (this.next < this.count) return null;
    const complete = this.parts.join("");
    this.reset();
    return complete;
  }
}
export const shouldAcceptSequence = (last, next) => ((next - last) & 0xffff) > 0 && ((next - last) & 0xffff) < 0x8000;
export const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));

/**
 * Computes an end-of-text edit that transforms `previous` into `next`.
 * The host caret is intentionally treated as being after the mirrored text:
 * retain the common prefix, backspace the old tail, then append the new tail.
 * Code points (rather than UTF-16 code units) keep emoji and other astral
 * characters atomic when a mobile browser edits the draft.
 */
export function reconcileTextEdit(previous = "", next = "") {
  const before = Array.from(String(previous));
  const after = Array.from(String(next));
  let prefix = 0;
  while (prefix < before.length && prefix < after.length && before[prefix] === after[prefix]) prefix++;
  return {
    deleteCount: before.length - prefix,
    insert: after.slice(prefix).join(""),
  };
}

// Secure-desktop text is deliberately narrower than ordinary IME editing:
// only explicit typing/composition is accepted, and every Unicode scalar is
// emitted as its own bounded packet. Paste and replacement/autocorrect events
// are ignored so credential text is never retained as an editable draft.
export function secureTextUnits(value = "", inputType = "insertText") {
  if (inputType !== "insertText" && inputType !== "insertCompositionText") return [];
  return Array.from(String(value));
}
// The synthetic overlay remains visible while Pointer Lock or the remote app
// hides the Windows cursor sprite. Position/in-bounds are authoritative; the
// OS visibility bit describes the captured sprite, not overlay usability.
export const shouldShowCursorOverlay = ({ pointerMode, synced, inBounds }) =>
  pointerMode === "desktop" && synced && inBounds;
export const shouldShowPredictedCursor = ({ pointerMode, inputReady }) =>
  pointerMode === "desktop" && inputReady;
export const canMoveAbsoluteCursor = ({ pointerMode, inputReady }, x, y) =>
  pointerMode === "desktop" && inputReady &&
  Number.isFinite(x) && Number.isFinite(y) &&
  x >= 0 && x <= 1 && y >= 0 && y <= 1;
export const shouldCaptureKeyboard = ({ inputReady, pointerMode, locked, surfaceFocused,
  keyboardFocused, localControlFocused }) =>
  inputReady && (locked || surfaceFocused || keyboardFocused ||
    (pointerMode === "desktop" && !localControlFocused));
export const shouldCenterCameraOnCursor = ({ zoom, pointerMode, synced, locallyAuthoritative }) =>
  zoom > 1 && pointerMode === "desktop" && (synced || locallyAuthoritative);
export const cursorDriftPixels = (state, cursor, remote) => {
  if (!state?.inBounds) return Number.POSITIVE_INFINITY;
  const target = cursor.absolutePoint();
  return Math.hypot((state.x - target.x) * Math.max(1, remote.width) / 65535,
    (state.y - target.y) * Math.max(1, remote.height) / 65535);
};
export const shouldCorrectCursorDrift = ({ pointerMode, locallyAuthoritative, state, cursor, remote, now, lastCorrectionAt, tolerancePx = 6, intervalMs = 100 }) =>
  pointerMode === "desktop" && locallyAuthoritative && now - lastCorrectionAt >= intervalMs &&
  cursorDriftPixels(state, cursor, remote) > tolerancePx;
const i16 = v => clamp(Math.round(v), -32768, 32767);

export const CURSOR_STATE = { ACK_VALID: 1, IN_BOUNDS: 2, VISIBLE: 4 };
export function decodeCursorState(data) {
  const bytes = data instanceof Uint8Array
    ? data
    : ArrayBuffer.isView(data)
      ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
      : data instanceof ArrayBuffer ? new Uint8Array(data) : null;
  if (!bytes || bytes.byteLength !== 8 || bytes[0] !== 128) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const flags = view.getUint8(7);
  return {
    ackSequence: view.getUint16(1, true),
    x: view.getUint16(3, true),
    y: view.getUint16(5, true),
    hasAck: Boolean(flags & CURSOR_STATE.ACK_VALID),
    inBounds: Boolean(flags & CURSOR_STATE.IN_BOUNDS),
    visible: Boolean(flags & CURSOR_STATE.VISIBLE),
  };
}

/**
 * Input state plus an ordered control queue. Control events (keys, buttons,
 * text, clipboard, release) are never dropped while the transport is alive:
 * when it is congested they wait in order and flush when it drains. Only
 * pointer motion is coalesced. When the transport is down (or input is gated)
 * nothing is queued into the dead session; the next live event is preceded by
 * a release-all so the host never keeps keys from the previous session.
 *
 * controlState() returns "open" (send now), "busy" (congested: queue) or
 * "down" (dead or gated: discard and require release-all on resume).
 */
export class InputController {
  constructor({ sendControl = () => false, sendMotion = () => false, motionBuffered = () => 0, backlogLimit = 64,
    controlState = () => "open", scheduleFlush = callback => setTimeout(callback, 16), maxQueuedBytes = 4 * 1024 * 1024 } = {}) {
    Object.assign(this, { sendControl, sendMotion, motionBuffered, backlogLimit, controlState, scheduleFlush, maxQueuedBytes,
      held: new Set(), buttonMask: 0, seq: 0, lastSentSequence: null, pending: [0, 0, 0, 0], pendingAbsolute: null,
      queue: [], queuedBytes: 0, needsRelease: false, flushArmed: false, scancode: false,
      stats: { sent: 0, coalesced: 0, dropped: 0, controlSent: 0, controlDropped: 0, controlQueued: 0, releaseResync: 0 } });
  }
  /** Accepts a control packet for in-order delivery; false only when the transport is down. */
  sendReliable(message) {
    if (!message) return false;
    if (this.controlState() === "down") { this.discardControl(1); return false; }
    this.queue.push(message); this.queuedBytes += message.byteLength;
    if (this.queuedBytes > this.maxQueuedBytes) { this.discardControl(0); return false; }
    if (this.queue.length > 1) this.stats.controlQueued++;
    this.flushControl();
    return true;
  }
  /** Sends queued control packets in order while the transport accepts them. */
  flushControl() {
    this.flushArmed = false;
    while (this.queue.length) {
      const state = this.controlState();
      if (state === "down") { this.discardControl(0); return false; }
      if (state !== "open") break;
      if (this.needsRelease) {
        if (!this.trySend(packet.releaseAll())) break;
        this.needsRelease = false; this.stats.releaseResync++;
      }
      if (!this.trySend(this.queue[0])) break;
      this.queuedBytes -= this.queue.shift().byteLength; this.stats.controlSent++;
    }
    if (this.queue.length) { this.armFlush(); return false; }
    if (this.pendingAbsolute || this.pending.some(Boolean)) this.flushMotion();
    return true;
  }
  trySend(message) { try { return Boolean(this.sendControl(message)); } catch { return false; } }
  armFlush() { if (this.flushArmed) return; this.flushArmed = true; this.scheduleFlush(() => this.flushControl()); }
  /** Drops queued control for a dead/gated session; the next live event resynchronizes with release-all. */
  discardControl(extra = 0) {
    this.stats.controlDropped += this.queue.length + extra;
    this.queue = []; this.queuedBytes = 0; this.needsRelease = true;
  }
  /** The transport died: forget local state; the host releases on its side too. */
  transportDown() { this.discardControl(0); this.held.clear(); this.buttonMask = 0; this.clearMotion(); }
  get controlBacklog() { return this.queue.length; }
  key(vk, up) { if (up) this.held.delete(vk); else this.held.add(vk); return this.sendReliable(packet.key(vk, up)); }
  /** Physical key by KeyboardEvent.code: scan code on capable hosts, VK otherwise. */
  physicalKey(code, up) {
    const key = keyDescriptor(code);
    if (!key) return false;
    if (!this.scancode || !key.scan) {
      if (!this.scancode) return key.legacyVk === undefined ? false : this.key(key.legacyVk, up);
      if (!key.vk) return false;
    }
    const id = `code:${code}`;
    if (up) this.held.delete(id); else this.held.add(id);
    return this.sendReliable(packet.scanKey(key.scan, key.extended, key.vk, up));
  }
  clipboard(text, id, options) { const packets = clipboardPackets(text, id, options); if (!packets) return false; return packets.every(message => this.sendReliable(message)); }
  text(value) { const message = packet.text(value); return message ? this.sendReliable(message) : false; }
  buttons(mask) { this.buttonMask = mask; return this.sendReliable(packet.buttons(mask)); }
  buttonsAt(x, y, mask) { this.clearMotion(); const sequence = this.seq, message = packet.buttonsAt(sequence, clamp(Math.round(x),0,65535), clamp(Math.round(y),0,65535), mask); if (!this.sendReliable(message)) return false; this.lastSentSequence = sequence; this.seq = (sequence + 1) & 0xffff; this.buttonMask = mask; return true; }
  cursorSync() { this.clearMotion(); const sequence = this.seq; if (!this.sendReliable(packet.cursorSync(sequence))) return false; this.lastSentSequence = sequence; this.seq = (sequence + 1) & 0xffff; return true; }
  motion(dx, dy, wx = 0, wy = 0) { if (this.pendingAbsolute) { this.pendingAbsolute = null; } else if (this.pending.some(Boolean)) this.stats.coalesced++; this.pending[0] += dx; this.pending[1] += dy; this.pending[2] += wx; this.pending[3] += wy; }
  motionNow(dx, dy, wx = 0, wy = 0) { this.motion(dx, dy, wx, wy); return this.flushMotion(); }
  absolute(x, y) { if (this.pendingAbsolute) this.stats.coalesced++; this.pending = [0, 0, 0, 0]; this.pendingAbsolute = [clamp(Math.round(x),0,65535), clamp(Math.round(y),0,65535)]; }
  clearMotion() { this.pending = [0, 0, 0, 0]; this.pendingAbsolute = null; }
  hasPendingMotion() { return Boolean(this.pendingAbsolute) || this.pending.some(Boolean); }
  // Motion never overtakes queued control (a click/drag must not be reordered).
  // A congested send keeps the coalesced motion/wheel pending rather than
  // losing wheel distance or the final pointer position.
  flushMotion() {
    const absolute = this.pendingAbsolute, p = this.pending;
    if (!absolute && !p.some(Boolean)) return false;
    if (this.controlState() === "down") { this.clearMotion(); return false; }
    if (this.queue.length) return false;
    this.clearMotion();
    const sequence = this.seq, message = absolute ? packet.absolute(sequence, ...absolute) : packet.motion(sequence, ...p.map(i16));
    let sent = false;
    if (this.motionBuffered() <= this.backlogLimit) { try { sent = Boolean(this.sendMotion(message)); } catch { sent = false; } }
    if (!sent) { this.stats.dropped++; this.pending = p; this.pendingAbsolute = absolute; return false; }
    this.lastSentSequence = sequence; this.seq = (sequence + 1) & 0xffff; this.stats.sent++; return true;
  }
  releaseAll(send = true) { this.held.clear(); this.buttonMask = 0; this.clearMotion(); if (send) return this.sendReliable(packet.releaseAll()); this.discardControl(0); return false; }
}

/** Reconciles authoritative host cursor samples without rewinding newer input. */
export class CursorReconciler {
  constructor(cursor) { this.cursor = cursor; this.reset(); }
  reset() { this.synced = false; this.pending = false; this.visible = false; this.inBounds = false; this.lastSent = null; this.lastAck = null; this.feedbackAt = null; }
  noteSent(sequence) { if (sequence === null || sequence === undefined) return; this.lastSent = sequence & 0xffff; this.pending = true; }
  apply(state, { touchActive = false, preserveCursor = false, now = performance.now() } = {}) {
    if (!state) return false;
    this.feedbackAt = now;
    this.visible = state.visible;
    this.inBounds = state.inBounds;
    let currentAck = false;
    if (state.hasAck) {
      if (this.lastAck === null || state.ackSequence === this.lastAck || shouldAcceptSequence(this.lastAck, state.ackSequence)) this.lastAck = state.ackSequence;
      currentAck = this.lastSent === null || state.ackSequence === this.lastSent || shouldAcceptSequence(this.lastSent, state.ackSequence);
      if (currentAck) this.pending = false;
    }
    const authoritative = currentAck || (!touchActive && !this.pending && !state.hasAck);
    if (!authoritative) return false;
    this.synced = true;
    if (!state.inBounds) return false;
    if (!preserveCursor) this.cursor.setNormalized(state.x / 65535, state.y / 65535);
    return true;
  }
}

export class Viewport {
  constructor(width, height, content = { x: 0, y: 0, width, height }) { this.zoom = 1; this.x = this.y = 0; this.resize(width, height); this.setContent(content); }
  resize(width, height) { this.width = Math.max(1, width); this.height = Math.max(1, height); return this.clamp(); }
  setContent({ x, y, width, height }) { this.content = { x, y, width: Math.max(1, width), height: Math.max(1, height) }; return this.clamp(); }
  axis(value, start, size, viewport) { const scaled = size * this.zoom; const result = scaled < viewport ? (viewport - scaled) / 2 - start * this.zoom : clamp(value, viewport - (start + size) * this.zoom, -start * this.zoom); return result === 0 ? 0 : result; }
  clamp() { const c = this.content || { x: 0, y: 0, width: this.width, height: this.height }; this.x = this.axis(this.x, c.x, c.width, this.width); this.y = this.axis(this.y, c.y, c.height, this.height); return this.snapshot(); }
  setZoom(next, anchor = { x: this.width / 2, y: this.height / 2 }) { const old = this.zoom; this.zoom = clamp(next, 1, 4); this.x = anchor.x - (anchor.x - this.x) * this.zoom / old; this.y = anchor.y - (anchor.y - this.y) * this.zoom / old; if (this.zoom === 1) this.x = this.y = 0; return this.clamp(); }
  centerOnNormalized({ x, y }) { const c = this.content; this.x = this.width / 2 - (c.x + clamp(x, 0, 1) * c.width) * this.zoom; this.y = this.height / 2 - (c.y + clamp(y, 0, 1) * c.height) * this.zoom; return this.clamp(); }
  pan(dx, dy) { this.x += dx; this.y += dy; return this.clamp(); }
  snapshot() { return { zoom: this.zoom, x: this.x, y: this.y }; }
}

/**
 * A latency-hiding estimate of the remote pointer, expressed in normalized
 * media coordinates. It gives touch users immediate visual feedback and a
 * stable zoom anchor without waiting for the next encoded desktop frame.
 */
export class RemoteCursor {
  constructor(x = 0.5, y = 0.5) { this.setNormalized(x, y); }
  reset() { return this.setNormalized(0.5, 0.5); }
  setNormalized(x, y) { this.x = clamp(x, 0, 1); this.y = clamp(y, 0, 1); return this.snapshot(); }
  absolutePoint() { return { x: Math.round(this.x * 65535), y: Math.round(this.y * 65535) }; }
  moveScreen(dx, dy, viewport, content = viewport.content) {
    const { zoom } = viewport.snapshot();
    this.x = clamp(this.x + dx / (Math.max(1, content.width) * zoom), 0, 1);
    this.y = clamp(this.y + dy / (Math.max(1, content.height) * zoom), 0, 1);
    return this.snapshot();
  }
  remoteDeltaForScreen(dx, dy, viewport, remote, content = viewport.content) {
    const { zoom } = viewport.snapshot();
    return {
      dx: dx * Math.max(1, remote.width) / (Math.max(1, content.width) * zoom),
      dy: dy * Math.max(1, remote.height) / (Math.max(1, content.height) * zoom),
    };
  }
  setFromScreen(point, viewport, content = viewport.content) {
    const { zoom, x, y } = viewport.snapshot();
    return this.setNormalized(
      ((point.x - x) / zoom - content.x) / Math.max(1, content.width),
      ((point.y - y) / zoom - content.y) / Math.max(1, content.height),
    );
  }
  screenPoint(viewport, content = viewport.content) {
    const { zoom, x, y } = viewport.snapshot();
    return {
      x: x + (content.x + this.x * content.width) * zoom,
      y: y + (content.y + this.y * content.height) * zoom,
    };
  }
  snapshot() { return { x: this.x, y: this.y }; }
}

export class TouchGesture {
  constructor({ moveThreshold = 4, pinchThreshold = 4, doubleTapMs = 300, doubleTapDistance = 24 } = {}) {
    Object.assign(this, { moveThreshold, pinchThreshold, doubleTapMs, doubleTapDistance });
    this.points = new Map(); this.origins = new Map(); this.lastTap = null;
    this.resetGesture();
  }
  down(id, point, now = performance.now()) {
    if (this.cancelled || this.points.size >= 2) return [];
    const actions = [];
    this.points.set(id, point); this.origins.set(id, point);
    if (this.points.size === 1) {
      this.dragged = false; this.seenSecond = false; this.mode = null;
      this.baseline = null; this.suppressUntilEmpty = false; this.twoFingerTap = false;
      const recent = this.lastTap && now - this.lastTap.at <= this.doubleTapMs
        && Math.hypot(point.x - this.lastTap.point.x, point.y - this.lastTap.point.y) <= this.doubleTapDistance;
      if (recent) { this.dragActive = true; this.lastTap = null; actions.push({ type: "drag-start" }); }
      else { this.dragActive = false; if (this.lastTap && now - this.lastTap.at > this.doubleTapMs) this.lastTap = null; }
      return actions;
    }
    const [a,b] = [...this.points.values()];
    if (this.dragActive) { this.dragActive = false; actions.push({ type: "drag-end" }); }
    this.seenSecond = this.suppressUntilEmpty = true; this.mode = null;
    this.twoFingerTap = actions.length === 0;
    this.baseline = { dist: Math.hypot(a.x-b.x,a.y-b.y), mid: {x:(a.x+b.x)/2,y:(a.y+b.y)/2} };
    actions.push({type:"suppress-motion"});
    return actions;
  }
  move(id, point) { if (this.cancelled || !this.points.has(id)) return []; const old = this.points.get(id); this.points.set(id, point); if (this.points.size === 1) { const origin=this.origins.get(id); if (Math.hypot(point.x-origin.x,point.y-origin.y) >= this.moveThreshold) this.dragged=true; return this.suppressUntilEmpty ? [] : [{ type: "motion", dx: point.x-old.x, dy: point.y-old.y }]; }
    const [a,b] = [...this.points.values()], keys=[...this.points.keys()]; const previous = id === keys[0] ? [old,b] : [a,old]; const dist=Math.hypot(a.x-b.x,a.y-b.y), oldDist=Math.hypot(previous[0].x-previous[1].x,previous[0].y-previous[1].y), mid={x:(a.x+b.x)/2,y:(a.y+b.y)/2}, oldMid={x:(previous[0].x+previous[1].x)/2,y:(previous[0].y+previous[1].y)/2};
    if (!this.mode) { if (Math.abs(dist-this.baseline.dist) >= this.pinchThreshold) this.mode="pinch"; else if (Math.hypot(mid.x-this.baseline.mid.x,mid.y-this.baseline.mid.y) >= this.moveThreshold) this.mode="translate"; else return []; this.twoFingerTap=false; }
    return this.mode === "pinch" ? [{type:"pinch",scale:oldDist ? dist/oldDist : 1,anchor:mid}] : [{type:"translate",dx:mid.x-oldMid.x,dy:mid.y-oldMid.y}]; }
  up(id, now = performance.now()) {
    if (!this.points.has(id)) return [];
    const point = this.points.get(id);
    this.points.delete(id); this.origins.delete(id);
    if (this.points.size) return [];
    const actions = [];
    if (this.dragActive) { this.dragActive = false; this.lastTap = null; actions.push({ type: "drag-end" }); }
    else if (!this.cancelled && this.twoFingerTap) { this.lastTap = null; actions.push({ type: "right-tap" }); }
    else if (!this.cancelled && !this.seenSecond && !this.dragged) { this.lastTap = { at: now, point }; actions.push({ type: "tap" }); }
    else if (!this.cancelled && !this.seenSecond && this.dragged) { this.lastTap = null; actions.push({ type: "motion-end" }); }
    else this.lastTap = null;
    this.resetGesture();
    return actions;
  }
  cancel(id) {
    const actions = [];
    if (this.dragActive) { this.dragActive = false; actions.push({ type: "drag-end" }); }
    if (this.points.has(id)) { this.points.delete(id); this.origins.delete(id); }
    this.lastTap = null; this.twoFingerTap = false; this.cancelled = this.points.size > 0;
    this.mode = null; this.suppressUntilEmpty = this.points.size > 0;
    if (!this.points.size) this.resetGesture();
    return actions;
  }
  resetGesture() { this.points.clear(); this.origins.clear(); this.cancelled = false; this.seenSecond = false; this.dragged = false; this.dragActive = false; this.twoFingerTap = false; this.mode = null; this.baseline = null; this.suppressUntilEmpty = false; }
  reset() { this.lastTap = null; this.resetGesture(); }
}
