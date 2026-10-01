// Session chrome for the web receiver: the docked strip (top on desktop,
// bottom on touch), its anchored popovers, the settings slide-over and the
// fullscreen top-edge handle. Pure helpers are exported for tests; the DOM
// controller only moves focus and toggles visibility — every remote action is
// a callback supplied by receiver.js.

const CTRL = "ControlLeft", ALT = "AltLeft", SHIFT = "ShiftLeft", WIN = "MetaLeft";

/**
 * Shortcuts popover, top to bottom. `desktop: false` rows only appear on touch
 * (a desktop keyboard types those itself). Ctrl+Alt+Del is deliberately absent:
 * Windows rejects an injected secure attention sequence.
 */
export const SHORTCUTS = Object.freeze([
  { id: "copy", label: "Copy", keys: [CTRL, "KeyC"], desktop: false },
  { id: "paste", label: "Paste", keys: [CTRL, "KeyV"], desktop: false },
  { id: "cut", label: "Cut", keys: [CTRL, "KeyX"], desktop: false },
  { id: "undo", label: "Undo", keys: [CTRL, "KeyZ"], desktop: false },
  { id: "select-all", label: "Select all", keys: [CTRL, "KeyA"], desktop: false },
  { id: "close-tab", label: "Close tab", keys: [CTRL, "KeyW"] },
  { id: "new-tab", label: "New tab", keys: [CTRL, "KeyT"] },
  { id: "new-window", label: "New window", keys: [CTRL, "KeyN"] },
  { id: "reopen-tab", label: "Reopen closed tab", keys: [CTRL, SHIFT, "KeyT"] },
  { id: "switch-window", label: "Switch window", keys: [ALT, "Tab"] },
  { id: "close-window", label: "Close window", keys: [ALT, "F4"] },
  { id: "start", label: "Start menu", keys: [WIN] },
  { id: "show-desktop", label: "Show desktop", keys: [WIN, "KeyD"] },
  { id: "task-manager", label: "Task Manager", keys: [CTRL, SHIFT, "Escape"] },
].map(Object.freeze));

const KEY_NAMES = Object.freeze({
  [CTRL]: "Ctrl", [ALT]: "Alt", [SHIFT]: "Shift", [WIN]: "Win", Escape: "Esc", Delete: "Del",
  PageUp: "PgUp", PageDown: "PgDn", ArrowLeft: "←", ArrowUp: "↑", ArrowRight: "→", ArrowDown: "↓",
});

export function keyName(code) {
  if (KEY_NAMES[code]) return KEY_NAMES[code];
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit\d$/.test(code)) return code.slice(5);
  return code;
}

/** "Ctrl+Shift+T", independent of the host keyboard layout. */
export function chordLabel(keys) { return keys.map(keyName).join("+"); }

/** Press every key in order, then release in reverse: [[code, up], ...]. */
export function chordSteps(keys) {
  return [...keys.map(code => [code, false]), ...[...keys].reverse().map(code => [code, true])];
}

export function shortcutsFor(layout) {
  return SHORTCUTS.filter(shortcut => layout === "touch" || shortcut.desktop !== false);
}

/**
 * Fullscreen top-edge reveal: the pointer must rest within `edgePx` of the top
 * for `delayMs` before the handle shows. Moving away cancels a pending show.
 */
export function createEdgeReveal({ delayMs = 300, edgePx = 4, schedule = setTimeout, cancel = clearTimeout, onArm }) {
  let timer = null;
  let armed = false;
  return {
    get armed() { return armed; },
    move(y, enabled) {
      if (!enabled || y > edgePx) {
        if (timer !== null) cancel(timer);
        timer = null;
        return;
      }
      if (armed || timer !== null) return;
      timer = schedule(() => { timer = null; armed = true; onArm?.(); }, delayMs);
    },
    reset() {
      if (timer !== null) cancel(timer);
      timer = null;
      armed = false;
    },
  };
}

/**
 * Places a popover next to its anchor inside the viewport with an 8 px
 * margin: below a top strip, above a bottom bar, centred on the anchor.
 */
export function popoverPosition({ anchor, popover, viewport, placement, gap = 6, margin = 8 }) {
  const centre = anchor.left + anchor.width / 2;
  const maxLeft = Math.max(margin, viewport.width - popover.width - margin);
  const left = Math.min(maxLeft, Math.max(margin, centre - popover.width / 2));
  if (placement === "above") {
    return { left, bottom: Math.max(margin, viewport.height - anchor.top + gap), maxHeight: Math.max(120, anchor.top - gap - margin) };
  }
  const top = anchor.bottom + gap;
  return { left, top, maxHeight: Math.max(120, viewport.height - top - margin) };
}

const FOCUSABLE = "button:not([disabled]), select:not([disabled]), input:not([disabled]), summary, [tabindex]:not([tabindex='-1'])";

/**
 * One popover (or the settings panel) open at a time. Outside presses land on
 * a scrim and only close; Esc closes. Focus goes into a panel only when it was
 * opened from the keyboard; otherwise it stays on (and returns to) the remote
 * input surface so keystrokes keep reaching the remote desktop.
 */
export function createPopoverController({ doc = document, scrim, surface, layout, onChange }) {
  let current = null; // { name, panel, anchor, viaKeyboard }

  function anchorPlacement() { return layout() === "touch" ? "above" : "below"; }

  function place(panel, anchor) {
    if (!anchor || panel.dataset.placement === "panel") return;
    panel.style.left = panel.style.top = panel.style.bottom = panel.style.maxHeight = "";
    const rect = anchor.getBoundingClientRect();
    const box = panel.getBoundingClientRect();
    const view = doc.documentElement;
    const position = popoverPosition({ anchor: rect, popover: { width: box.width, height: box.height },
      viewport: { width: view.clientWidth, height: view.clientHeight }, placement: anchorPlacement() });
    panel.style.left = `${Math.round(position.left)}px`;
    if (position.top !== undefined) panel.style.top = `${Math.round(position.top)}px`;
    if (position.bottom !== undefined) panel.style.bottom = `${Math.round(position.bottom)}px`;
    panel.style.maxHeight = `${Math.round(position.maxHeight)}px`;
  }

  function returnFocus(target) {
    const active = doc.activeElement;
    // A touch keyboard sink keeps the soft keyboard up across popovers.
    if (active?.id === "keyboard-input") return;
    (target || surface)?.focus?.({ preventScroll: true });
  }

  function close({ restore = true } = {}) {
    if (!current) return false;
    const { panel, anchor, viaKeyboard, name } = current;
    current = null;
    panel.hidden = true;
    panel.classList.remove("open");
    anchor?.setAttribute("aria-expanded", "false");
    if (scrim) scrim.hidden = true;
    onChange?.(null, name);
    if (restore) returnFocus(viaKeyboard ? anchor : null);
    return true;
  }

  function open(name, panel, anchor, viaKeyboard = false) {
    if (current?.name === name) return close();
    if (current) close({ restore: false });
    current = { name, panel, anchor, viaKeyboard };
    onChange?.(name, null);
    panel.hidden = false;
    panel.classList.add("open");
    anchor?.setAttribute("aria-expanded", "true");
    if (scrim) scrim.hidden = false;
    place(panel, anchor);
    if (viaKeyboard) panel.querySelector(FOCUSABLE)?.focus({ preventScroll: true });
    else returnFocus(null);
    return true;
  }

  function reposition() { if (current) place(current.panel, current.anchor); }

  scrim?.addEventListener("pointerdown", event => { event.preventDefault(); event.stopPropagation(); close(); });
  // Capture phase: Esc closes the open panel before the remote keyboard sees it.
  doc.addEventListener("keydown", event => {
    if (!current) return;
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopImmediatePropagation();
      close();
      return;
    }
    if ((event.key === "ArrowDown" || event.key === "ArrowUp") && current.panel.contains(doc.activeElement)
        && doc.activeElement?.tagName === "BUTTON") {
      const items = [...current.panel.querySelectorAll("button:not([disabled])")].filter(item => item.offsetParent !== null);
      const index = items.indexOf(doc.activeElement);
      if (index < 0 || !items.length) return;
      event.preventDefault();
      items[(index + (event.key === "ArrowDown" ? 1 : items.length - 1)) % items.length].focus();
    }
  }, true);

  return {
    open, close, reposition,
    get current() { return current?.name ?? null; },
    isOpen(name) { return current?.name === name; },
  };
}
