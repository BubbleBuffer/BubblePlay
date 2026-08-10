export const packet = {
  key: (vk, up) => Uint8Array.of(up ? 1 : 0, vk & 255, vk >> 8), buttons: mask => Uint8Array.of(4, mask), releaseAll: () => Uint8Array.of(6),
  motion: (seq, dx, dy, wheelX = 0, wheelY = 0) => { const d = new Uint8Array(11), v = new DataView(d.buffer); v.setUint8(0, 7); v.setUint16(1, seq, true); v.setInt16(3, dx, true); v.setInt16(5, dy, true); v.setInt16(7, wheelX, true); v.setInt16(9, wheelY, true); return d; },
  absolute: (seq, x, y) => { const d = new Uint8Array(7), v = new DataView(d.buffer); v.setUint8(0, 8); v.setUint16(1, seq, true); v.setUint16(3, x, true); v.setUint16(5, y, true); return d; },
};
export const shouldAcceptSequence = (last, next) => ((next - last) & 0xffff) > 0 && ((next - last) & 0xffff) < 0x8000;
export const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
const i16 = v => clamp(Math.round(v), -32768, 32767);

export class InputController {
  constructor({ sendControl = () => false, sendMotion = () => false, motionBuffered = () => 0, backlogLimit = 64 } = {}) { Object.assign(this, { sendControl, sendMotion, motionBuffered, backlogLimit, held: new Set(), buttonMask: 0, seq: 0, pending: [0, 0, 0, 0], pendingAbsolute: null, stats: { sent: 0, coalesced: 0, dropped: 0 } }); }
  key(vk, up) { if (up) this.held.delete(vk); else this.held.add(vk); return this.sendControl(packet.key(vk, up)); }
  buttons(mask) { this.buttonMask = mask; return this.sendControl(packet.buttons(mask)); }
  motion(dx, dy, wx = 0, wy = 0) { if (this.pendingAbsolute) { this.pendingAbsolute = null; } else if (this.pending.some(Boolean)) this.stats.coalesced++; this.pending[0] += dx; this.pending[1] += dy; this.pending[2] += wx; this.pending[3] += wy; }
  absolute(x, y) { if (this.pendingAbsolute) this.stats.coalesced++; this.pending = [0, 0, 0, 0]; this.pendingAbsolute = [clamp(Math.round(x),0,65535), clamp(Math.round(y),0,65535)]; }
  clearMotion() { this.pending = [0, 0, 0, 0]; this.pendingAbsolute = null; }
  flushMotion() { const absolute = this.pendingAbsolute, p = this.pending; this.clearMotion(); if (!absolute && !p.some(Boolean)) return false; const message = absolute ? packet.absolute(this.seq, ...absolute) : packet.motion(this.seq, ...p.map(i16)); if (this.motionBuffered() > this.backlogLimit || !this.sendMotion(message)) { this.stats.dropped++; return false; } this.seq = (this.seq + 1) & 0xffff; this.stats.sent++; return true; }
  releaseAll(send = true) { this.held.clear(); this.buttonMask = 0; this.clearMotion(); return send ? this.sendControl(packet.releaseAll()) : false; }
}

export class Viewport {
  constructor(width, height, content = { x: 0, y: 0, width, height }) { this.zoom = 1; this.x = this.y = 0; this.resize(width, height); this.setContent(content); }
  resize(width, height) { this.width = Math.max(1, width); this.height = Math.max(1, height); return this.clamp(); }
  setContent({ x, y, width, height }) { this.content = { x, y, width: Math.max(1, width), height: Math.max(1, height) }; return this.clamp(); }
  axis(value, start, size, viewport) { const scaled = size * this.zoom; const result = scaled < viewport ? (viewport - scaled) / 2 - start * this.zoom : clamp(value, viewport - (start + size) * this.zoom, -start * this.zoom); return result === 0 ? 0 : result; }
  clamp() { const c = this.content || { x: 0, y: 0, width: this.width, height: this.height }; this.x = this.axis(this.x, c.x, c.width, this.width); this.y = this.axis(this.y, c.y, c.height, this.height); return this.snapshot(); }
  setZoom(next, anchor = { x: this.width / 2, y: this.height / 2 }) { const old = this.zoom; this.zoom = clamp(next, 1, 4); this.x = anchor.x - (anchor.x - this.x) * this.zoom / old; this.y = anchor.y - (anchor.y - this.y) * this.zoom / old; if (this.zoom === 1) this.x = this.y = 0; return this.clamp(); }
  pan(dx, dy) { this.x += dx; this.y += dy; return this.clamp(); }
  snapshot() { return { zoom: this.zoom, x: this.x, y: this.y }; }
}

export class TouchGesture {
  constructor({ moveThreshold = 8, pinchThreshold = 6 } = {}) { this.moveThreshold = moveThreshold; this.pinchThreshold = pinchThreshold; this.points = new Map(); this.origins = new Map(); this.cancelled = false; this.seenSecond = false; this.dragged = false; this.mode = null; this.baseline = null; this.suppressUntilEmpty = false; }
  down(id, point) { if (this.cancelled || this.points.size >= 2) return []; this.points.set(id, point); this.origins.set(id, point); if (this.points.size === 2) { const [a,b] = [...this.points.values()]; this.seenSecond = this.suppressUntilEmpty = true; this.mode = null; this.baseline = { dist: Math.hypot(a.x-b.x,a.y-b.y), mid: {x:(a.x+b.x)/2,y:(a.y+b.y)/2} }; return [{type:"suppress-motion"}]; } return []; }
  move(id, point) { if (this.cancelled || !this.points.has(id)) return []; const old = this.points.get(id); this.points.set(id, point); if (this.points.size === 1) { const origin=this.origins.get(id); if (Math.hypot(point.x-origin.x,point.y-origin.y) >= this.moveThreshold) this.dragged=true; return this.suppressUntilEmpty ? [] : [{ type: "motion", dx: point.x-old.x, dy: point.y-old.y }]; }
    const [a,b] = [...this.points.values()], keys=[...this.points.keys()]; const previous = id === keys[0] ? [old,b] : [a,old]; const dist=Math.hypot(a.x-b.x,a.y-b.y), oldDist=Math.hypot(previous[0].x-previous[1].x,previous[0].y-previous[1].y), mid={x:(a.x+b.x)/2,y:(a.y+b.y)/2}, oldMid={x:(previous[0].x+previous[1].x)/2,y:(previous[0].y+previous[1].y)/2};
    if (!this.mode) { if (Math.abs(dist-this.baseline.dist) >= this.pinchThreshold) this.mode="pinch"; else if (Math.hypot(mid.x-this.baseline.mid.x,mid.y-this.baseline.mid.y) >= this.moveThreshold) this.mode="translate"; else return []; }
    return this.mode === "pinch" ? [{type:"pinch",scale:oldDist ? dist/oldDist : 1,anchor:mid}] : [{type:"translate",dx:mid.x-oldMid.x,dy:mid.y-oldMid.y}]; }
  up(id) { if (!this.points.has(id)) return []; this.points.delete(id); this.origins.delete(id); const tap=!this.cancelled&&!this.seenSecond&&!this.dragged&&this.points.size===0; if (!this.points.size) this.reset(); return tap ? [{type:"tap"}] : []; }
  cancel(id) { if (this.points.has(id)) { this.points.delete(id); this.origins.delete(id); } this.cancelled = this.points.size > 0; this.mode = null; if (!this.points.size) this.reset(); return []; }
  reset() { this.points.clear(); this.origins.clear(); this.cancelled = false; this.seenSecond = false; this.dragged = false; this.mode = null; this.baseline = null; this.suppressUntilEmpty = false; }
}
