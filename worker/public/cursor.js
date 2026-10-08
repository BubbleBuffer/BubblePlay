// Semantic cursor feedback. Pixels and themes belong to the client platform.
// Wire IDs are shared with host/Android in fixtures/cursor-types.json.
export const CURSOR_TYPE_PACKET = 129;
export const CursorKind = Object.freeze({
  Arrow: 0, Text: 1, Pointer: 2, Crosshair: 3, Move: 4,
  ResizeNS: 5, ResizeEW: 6, ResizeNESW: 7, ResizeNWSE: 8,
  Wait: 9, Progress: 10, NotAllowed: 11, Help: 12, UpArrow: 13, Hidden: 14,
});
const nativeCursors = Object.freeze([
  "default", "text", "pointer", "crosshair", "move", "ns-resize", "ew-resize",
  "nesw-resize", "nwse-resize", "wait", "progress", "not-allowed", "help", "default", "none",
]);
export const normalizeCursorKind = kind =>
  Number.isInteger(kind) && kind >= 0 && kind < nativeCursors.length ? kind : CursorKind.Arrow;
export const nativeCursorForKind = kind => nativeCursors[normalizeCursorKind(kind)];

export function decodeCursorType(data) {
  const bytes = data instanceof Uint8Array ? data : ArrayBuffer.isView(data)
    ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
    : data instanceof ArrayBuffer ? new Uint8Array(data) : null;
  if (!bytes || bytes.byteLength !== 2 || bytes[0] !== CURSOR_TYPE_PACKET) return null;
  return normalizeCursorKind(bytes[1]);
}

const arrow = '<path d="M2 2v22l6.2-5.5 5 11 3.8-1.8-5-10.7h9L2 2Z" fill="white" stroke="black" stroke-width="2" stroke-linejoin="round"/>';
const line = d => `<path d="${d}" fill="none" stroke="black" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/><path d="${d}" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>`;
const spinner = (x, y, r) => `<circle cx="${x}" cy="${y}" r="${r}" fill="black" stroke="white" stroke-width="1"/><g class="cursor-spinner"><circle cx="${x}" cy="${y}" r="${r - 2}" fill="none" stroke="white" stroke-width="2" stroke-dasharray="${(r - 2) * 4} ${(r - 2) * 2.3}"/></g>`;

// Touch/pen have no native mouse pointer to restyle. Keep a small, outlined
// semantic glyph at the predicted click location, independent of video zoom.
export function cursorOverlayForKind(value) {
  const kind = normalizeCursorKind(value);
  let artwork, hotspot = { x: 12, y: 16 };
  switch (kind) {
    case CursorKind.Text:
      artwork = line("M7 5h10M7 27h10M12 5v22"); break;
    case CursorKind.Pointer:
      hotspot = { x: 10, y: 3 };
      artwork = '<path d="M10 17V5a2 2 0 0 1 4 0v9-3a2 2 0 0 1 4 0v4-2a2 2 0 0 1 4 0v10c0 4-3 6-7 6h-3c-2 0-3-1-4-3l-5-8c-1-2 1-4 3-2l4 3Z" fill="white" stroke="black" stroke-width="2" stroke-linejoin="round"/>'; break;
    case CursorKind.Crosshair:
      artwork = line("M12 4v24M1 16h22"); break;
    case CursorKind.Move:
      artwork = line("M12 4v24M1 16h22M8 8l4-4 4 4M8 24l4 4 4-4M5 12l-4 4 4 4M19 12l4 4-4 4"); break;
    case CursorKind.ResizeNS:
      artwork = line("M12 4v24M7 9l5-5 5 5M7 23l5 5 5-5"); break;
    case CursorKind.ResizeEW:
      artwork = line("M2 16h20M7 11l-5 5 5 5M17 11l5 5-5 5"); break;
    case CursorKind.ResizeNESW:
      artwork = line("M3 25 21 7M14 7h7v7M3 18v7h7"); break;
    case CursorKind.ResizeNWSE:
      artwork = line("M3 7 21 25M3 14V7h7M14 25h7v-7"); break;
    case CursorKind.Wait:
      artwork = spinner(12, 16, 10); break;
    case CursorKind.Progress:
      hotspot = { x: 2, y: 2 }; artwork = arrow + spinner(20, 24, 6); break;
    case CursorKind.NotAllowed:
      artwork = '<circle cx="12" cy="16" r="10" fill="white" stroke="black" stroke-width="2"/>' + line("M5 9 19 23"); break;
    case CursorKind.Help:
      hotspot = { x: 2, y: 2 };
      artwork = arrow + '<path d="M16 6c0-6 8-6 8-1 0 3-4 3-4 6M20 15v.2" fill="none" stroke="black" stroke-width="4" stroke-linecap="round"/><path d="M16 6c0-6 8-6 8-1 0 3-4 3-4 6M20 15v.2" fill="none" stroke="white" stroke-width="2" stroke-linecap="round"/>'; break;
    case CursorKind.UpArrow:
      hotspot = { x: 12, y: 3 };
      artwork = '<path d="M12 3 2 14h6v15h8V14h6L12 3Z" fill="white" stroke="black" stroke-width="2" stroke-linejoin="round"/>'; break;
    case CursorKind.Hidden:
      return { markup: "", hotspot, hidden: true };
    default:
      hotspot = { x: 2, y: 2 }; artwork = arrow;
  }
  return { markup: `<svg viewBox="0 0 24 32" aria-hidden="true">${artwork}</svg>`, hotspot, hidden: false };
}

export class BrowserCursorAppearance {
  constructor(surface, overlay) {
    this.surface = surface;
    this.overlay = overlay;
    this.reset();
  }
  reset() {
    this.kind = CursorKind.Arrow;
    this.renderedKind = null;
    this.received = 0;
    this.accepted = 0;
    this.lastReceivedKind = null;
    this.lastSource = null;
    // Clear hidden/busy native pointers immediately when a session ends.
    this.surface.style.cursor = "default";
  }
  receive(data, { source, controlOpen = false } = {}) {
    const kind = decodeCursorType(data);
    if (kind === null) return false;
    this.received++;
    this.lastReceivedKind = kind;
    this.lastSource = source || null;
    // The host publishes on both reliable lanes. Select one lane so an older
    // signaling packet cannot rewind a newer RTC kind (or vice versa).
    if (source && source !== (controlOpen ? "rtc" : "signaling")) return false;
    this.kind = kind;
    this.accepted++;
    return true;
  }
  snapshot() {
    return { received: this.received, accepted: this.accepted, kind: this.kind,
      lastReceivedKind: this.lastReceivedKind, source: this.lastSource,
      native: this.surface.style.cursor };
  }
  render({ pointerMode, inputReady, point, overlayVisible, mousePointerActive }) {
    // Work mode needs a usable local pointer even when Windows hides its
    // sprite. Cursor appearance must not override the existing input policy;
    // Game mode lets browser pointer lock control local cursor visibility.
    const kind = pointerMode === "desktop" && this.kind !== CursorKind.Hidden
      ? this.kind : CursorKind.Arrow;
    this.surface.style.cursor = pointerMode === "desktop" && inputReady
      ? nativeCursorForKind(kind) : "default";
    if (kind !== this.renderedKind) {
      const shape = cursorOverlayForKind(kind);
      this.overlay.innerHTML = shape.markup; // only static, locally authored SVG
      this.hotspot = shape.hotspot;
      this.hidden = shape.hidden;
      this.renderedKind = kind;
    }
    this.overlay.style.transform = `translate3d(${point.x - this.hotspot.x}px,${point.y - this.hotspot.y}px,0)`;
    this.overlay.classList.toggle("visible", overlayVisible && !mousePointerActive && !this.hidden);
  }
}
