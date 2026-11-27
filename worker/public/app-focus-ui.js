// App focus mode UI (docs/architecture/app-focus-mode.md): the "Show apps" strip button,
// the switcher popover with titles and icons, the "Show full desktop" button and the
// status label. Applies the controller's view to the viewport.
import { NOTICES } from "./app-focus.js";
import { STATES } from "./window-list-wire.js";

export const POPOVER = "pop-apps";
const NOTICE_LABELS = Object.freeze({
  [NOTICES.otherDisplay]: "On another display",
  [NOTICES.closed]: "App closed",
  [NOTICES.gone]: "App closed",
  [NOTICES.elevated]: "Elevated app",
  [NOTICES.refused]: "Focus refused",
});
const STATE_LABELS = Object.freeze({ [STATES.foreground]: "Current", [STATES.minimized]: "Minimized" });

function row(win, focused, url) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "pop-row app-row";
  button.setAttribute("role", "radio");
  button.setAttribute("aria-checked", String(focused));
  button.dataset.windowId = String(win.id);
  const icon = document.createElement("span");
  icon.className = "app-icon";
  if (url) { const img = document.createElement("img"); img.alt = ""; img.src = url; icon.append(img); }
  const title = document.createElement("span");
  title.className = "pop-row-title";
  title.textContent = win.title || win.process;
  const detail = document.createElement("span");
  detail.className = "pop-row-detail";
  detail.textContent = win.process;
  const trail = document.createElement("span");
  trail.className = "pop-row-trail";
  trail.textContent = STATE_LABELS[win.state] ?? "";
  button.append(icon, title, detail, trail);
  return button;
}

/** viewport is a FocusViewport; onViewChange() runs after its crop changed (relayout). */
export function createAppFocusUi({ appFocus, viewport, visual, streamArea, openPanel, popovers, onViewChange }) {
  const stripApps = document.getElementById("strip-apps");
  const stripFull = document.getElementById("strip-full-desktop");
  const status = document.getElementById("app-focus-status");
  const list = document.getElementById("app-list");
  const listStatus = document.getElementById("app-list-status");
  const fullRow = document.getElementById("app-full-desktop");
  let cropKey = "";

  function applyCrop(crop) {
    const key = JSON.stringify(crop);
    if (key === cropKey) return;
    cropKey = key;
    viewport.setFocus(crop);
    onViewChange();
  }
  function render() {
    const view = appFocus.view;
    applyCrop(view.crop);
    stripApps.hidden = !appFocus.capable;
    stripFull.hidden = !appFocus.active;
    fullRow.hidden = !appFocus.active;
    const label = NOTICE_LABELS[view.notice ?? appFocus.notice] ?? "";
    status.textContent = label;
    status.hidden = !label;
    if (!appFocus.capable && popovers.isOpen(POPOVER)) popovers.close({ restore: false });
    if (!popovers.isOpen(POPOVER)) return;
    listStatus.textContent = appFocus.windows.length ? "" : "No apps";
    list.replaceChildren(...appFocus.windows.map(win => {
      const button = row(win, win.id === appFocus.focusedId, win.icon ? appFocus.iconUrl(win.icon) : null);
      button.onclick = () => { appFocus.pick(win.id); popovers.close(); };
      return button;
    }));
  }
  function clipVisual() { visual.style.clipPath = viewport.clipPath(streamArea()); }

  for (const button of [stripApps, stripFull]) button.addEventListener("mousedown", e => e.preventDefault());
  stripApps.addEventListener("click", e => { e.stopPropagation(); openPanel(POPOVER, stripApps, e.detail === 0); });
  const leave = e => { e.stopPropagation(); appFocus.leave(); popovers.close(); };
  stripFull.addEventListener("click", leave);
  fullRow.addEventListener("click", leave);
  return {
    render,
    clipVisual,
    /** The popover controller's onChange(opened, closed). */
    onPopover(opened, closed) {
      if (opened === POPOVER) appFocus.openSwitcher();
      if (closed === POPOVER) appFocus.closeSwitcher();
    },
  };
}
