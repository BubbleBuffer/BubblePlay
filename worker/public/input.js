export const packet = {
  key: (vk, up) => Uint8Array.of(up ? 1 : 0, vk & 255, vk >> 8), buttons: mask => Uint8Array.of(4, mask), releaseAll: () => Uint8Array.of(6),
  text: value => { const bytes = new TextEncoder().encode(String(value)); if (!bytes.length || bytes.length > 512) return null; const out = new Uint8Array(bytes.length + 1); out[0] = 11; out.set(bytes, 1); return out; },
  motion: (seq, dx, dy, wheelX = 0, wheelY = 0) => { const d = new Uint8Array(11), v = new DataView(d.buffer); v.setUint8(0, 7); v.setUint16(1, seq, true); v.setInt16(3, dx, true); v.setInt16(5, dy, true); v.setInt16(7, wheelX, true); v.setInt16(9, wheelY, true); return d; },
  absolute: (seq, x, y) => { const d = new Uint8Array(7), v = new DataView(d.buffer); v.setUint8(0, 8); v.setUint16(1, seq, true); v.setUint16(3, x, true); v.setUint16(5, y, true); return d; },
  buttonsAt: (seq, x, y, mask) => { const d = new Uint8Array(8), v = new DataView(d.buffer); v.setUint8(0, 9); v.setUint16(1, seq, true); v.setUint16(3, x, true); v.setUint16(5, y, true); v.setUint8(7, mask); return d; },
  cursorSync: seq => Uint8Array.of(10, seq & 255, seq >> 8),
};
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

export class InputController {
  constructor({ sendControl = () => false, sendMotion = () => false, motionBuffered = () => 0, backlogLimit = 64 } = {}) { Object.assign(this, { sendControl, sendMotion, motionBuffered, backlogLimit, held: new Set(), buttonMask: 0, seq: 0, lastSentSequence: null, pending: [0, 0, 0, 0], pendingAbsolute: null, stats: { sent: 0, coalesced: 0, dropped: 0, controlSent: 0, controlDropped: 0 } }); }
  sendReliable(message) { const sent = Boolean(message && this.sendControl(message)); this.stats[sent ? "controlSent" : "controlDropped"]++; return sent; }
  key(vk, up) { if (up) this.held.delete(vk); else this.held.add(vk); return this.sendReliable(packet.key(vk, up)); }
  text(value) { const message = packet.text(value); return message ? this.sendReliable(message) : false; }
  buttons(mask) { this.buttonMask = mask; return this.sendReliable(packet.buttons(mask)); }
  buttonsAt(x, y, mask) { this.clearMotion(); const sequence = this.seq, message = packet.buttonsAt(sequence, clamp(Math.round(x),0,65535), clamp(Math.round(y),0,65535), mask); if (!this.sendReliable(message)) return false; this.lastSentSequence = sequence; this.seq = (sequence + 1) & 0xffff; this.buttonMask = mask; return true; }
  cursorSync() { this.clearMotion(); const sequence = this.seq; if (!this.sendReliable(packet.cursorSync(sequence))) return false; this.lastSentSequence = sequence; this.seq = (sequence + 1) & 0xffff; return true; }
  motion(dx, dy, wx = 0, wy = 0) { if (this.pendingAbsolute) { this.pendingAbsolute = null; } else if (this.pending.some(Boolean)) this.stats.coalesced++; this.pending[0] += dx; this.pending[1] += dy; this.pending[2] += wx; this.pending[3] += wy; }
  motionNow(dx, dy, wx = 0, wy = 0) { this.motion(dx, dy, wx, wy); return this.flushMotion(); }
  absolute(x, y) { if (this.pendingAbsolute) this.stats.coalesced++; this.pending = [0, 0, 0, 0]; this.pendingAbsolute = [clamp(Math.round(x),0,65535), clamp(Math.round(y),0,65535)]; }
  clearMotion() { this.pending = [0, 0, 0, 0]; this.pendingAbsolute = null; }
  flushMotion() { const absolute = this.pendingAbsolute, p = this.pending; this.clearMotion(); if (!absolute && !p.some(Boolean)) return false; const sequence = this.seq, message = absolute ? packet.absolute(sequence, ...absolute) : packet.motion(sequence, ...p.map(i16)); if (this.motionBuffered() > this.backlogLimit || !this.sendMotion(message)) { this.stats.dropped++; return false; } this.lastSentSequence = sequence; this.seq = (sequence + 1) & 0xffff; this.stats.sent++; return true; }
  releaseAll(send = true) { this.held.clear(); this.buttonMask = 0; this.clearMotion(); return send ? this.sendReliable(packet.releaseAll()) : false; }
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
