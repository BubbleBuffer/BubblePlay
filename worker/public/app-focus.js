// App focus mode, controller side (docs/architecture/app-focus-mode.md). The viewer
// picks one PC app, the host focuses it, and the existing stream is cropped to its
// window. This module holds the state, decides crop or full desktop, and sends the
// window-list messages; the DOM is app-focus-ui.js and the crop geometry is
// app-focus-viewport.js.
import { FAILURES, LIMITS, STATES, focusMessage, iconRequest, parseFocusFailed, parseIcon, parseWindowList,
  startMessage, stopMessage } from "./window-list-wire.js";

// Why the full desktop is shown while an app is focused (or why focus ended).
export const NOTICES = Object.freeze({ otherDisplay: "other-display", closed: "closed", ...FAILURES });
// Icons asked for and kept per session; the host caches about as many.
const ICONS_PER_WINDOW = 2;
const MAX_ICONS = LIMITS.maxWindows * ICONS_PER_WINDOW;

/**
 * Crop or full desktop for the focused window. `crop` is the window rectangle as fractions of
 * the streamed monitor (x, y, width, height in 0..1), so it is independent of the encoded size.
 */
export function focusView({ windows, focusedId, monitors, secure }) {
  const full = notice => ({ mode: "full", crop: null, notice });
  if (focusedId === null || secure) return full(null);
  const entry = windows.find(w => w.id === focusedId);
  if (!entry) return full(NOTICES.closed);
  const streamed = monitors.find(m => m.selected);
  if (!streamed || !(streamed.width > 0) || !(streamed.height > 0)) return full(null);
  if (entry.state === STATES.minimized) return full(null);
  if (entry.monitor !== streamed.selector) return full(NOTICES.otherDisplay);
  const left = Math.max(entry.rect.left - streamed.originX, 0);
  const top = Math.max(entry.rect.top - streamed.originY, 0);
  const right = Math.min(entry.rect.left + entry.rect.width - streamed.originX, streamed.width);
  const bottom = Math.min(entry.rect.top + entry.rect.height - streamed.originY, streamed.height);
  if (right <= left || bottom <= top) return full(NOTICES.otherDisplay);
  return { mode: "crop", notice: null, crop: { x: left / streamed.width, y: top / streamed.height,
    width: (right - left) / streamed.width, height: (bottom - top) / streamed.height } };
}

/**
 * deps: send(message) -> boolean; follow { capable(), shown(), send(boolean) } for follow active
 * window; onChange() after any state change; openSwitcher() to show the switcher again when the
 * focused app closes; createUrl/revokeUrl for icon pictures.
 */
export function createAppFocus({ send, follow, onChange = () => {}, openSwitcher = () => {},
  createUrl = bytes => URL.createObjectURL(new Blob([bytes], { type: "image/png" })), revokeUrl = url => URL.revokeObjectURL(url) }) {
  let capable = false, switcherOpen = false, started = false, secure = false;
  let windows = [], monitors = [], focusedId = null, notice = null, restoreFollow = false;
  const icons = new Map(); // hash -> object URL
  const asked = new Set();

  const view = () => focusView({ windows, focusedId, monitors, secure });
  const changed = () => { try { onChange(); } catch { /* a view update never breaks the session */ } };
  function sync() {
    const listening = capable && (switcherOpen || focusedId !== null);
    if (listening === started) return;
    if (send(listening ? startMessage() : stopMessage()) !== false) started = listening;
  }
  function askIcons() {
    for (const { icon } of windows) {
      if (!icon || icons.has(icon) || asked.has(icon) || asked.size >= MAX_ICONS) continue;
      if (send(iconRequest(icon)) !== false) asked.add(icon);
    }
  }
  function leave(next) {
    if (focusedId === null) return;
    focusedId = null;
    notice = next ?? null;
    if (restoreFollow) { restoreFollow = false; follow.send(true); }
    sync();
  }

  return {
    get capable() { return capable; },
    get active() { return focusedId !== null; },
    get focusedId() { return focusedId; },
    get windows() { return windows; },
    get notice() { return notice; },
    get view() { return view(); },
    iconUrl: hash => icons.get(hash) ?? null,
    /** Both sides negotiated window-list-v1. Losing it ends focus mode. */
    setCapable(on) { if (capable === on) return; capable = on; if (!on) this.reset(); changed(); },
    setMonitors(list) { monitors = list; changed(); },
    setSecure(on) { if (secure === on) return; secure = on; changed(); },
    openSwitcher() { switcherOpen = true; sync(); changed(); },
    closeSwitcher() { switcherOpen = false; sync(); changed(); },
    /** Focus an app from the latest list. */
    pick(id) {
      const entry = windows.find(w => w.id === id);
      const message = entry && focusMessage(id);
      if (!capable || !message) return false;
      if (focusedId === null && follow.capable() && follow.shown()) { restoreFollow = true; follow.send(false); }
      focusedId = id;
      notice = null;
      sync();
      send(message);
      changed();
      return true;
    },
    /** Show the full desktop again. */
    leave() { leave(null); changed(); },
    /** A new host session knows nothing of ours: drop everything without sending. */
    reset() {
      for (const url of icons.values()) { try { revokeUrl(url); } catch { /* already revoked */ } }
      icons.clear(); asked.clear();
      windows = []; focusedId = null; notice = null; restoreFollow = false; started = false; switcherOpen = false;
      changed();
    },
    onMessage(message) {
      if (!capable) return;
      const list = parseWindowList(message);
      if (list) {
        windows = list;
        if (focusedId !== null && !windows.some(w => w.id === focusedId)) { leave(NOTICES.closed); openSwitcher(); }
        askIcons();
        return changed();
      }
      const failure = parseFocusFailed(message);
      if (failure) {
        if (failure.id === focusedId) leave(failure.reason);
        return changed();
      }
      const icon = parseIcon(message);
      if (icon && asked.has(icon.hash) && !icons.has(icon.hash)) {
        icons.set(icon.hash, createUrl(icon.bytes));
        changed();
      }
    },
  };
}
