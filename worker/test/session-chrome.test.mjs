import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  SETTINGS_KEY, SETTINGS_VERSION, DEFAULT_SETTINGS,
  loadSettings, normalizeSettings, qualityStorageView, saveSettings,
} from "../public/settings.js";
import {
  SHORTCUTS, chordLabel, chordSteps, createEdgeReveal, keyName, popoverPosition, shortcutsFor,
} from "../public/chrome.js";
import { parseQualityPreset } from "../public/transport.js";

function memoryStorage(entries = {}) {
  const map = new Map(Object.entries(entries));
  return {
    getItem: key => map.has(key) ? map.get(key) : null,
    setItem: (key, value) => { map.set(key, String(value)); },
    map,
  };
}
const throwingStorage = {
  getItem() { throw new Error("SecurityError"); },
  setItem() { throw new Error("QuotaExceededError"); },
};

test("settings live under one namespaced, versioned key", () => {
  assert.equal(SETTINGS_KEY, "bubbleplay.settings");
  const storage = memoryStorage();
  const settings = { ...DEFAULT_SETTINGS, quality: "saver", scrollSpeed: 1.5, keyboardLock: false, defaultMonitor: "\\\\.\\DISPLAY2" };
  assert.equal(saveSettings(storage, settings), true);
  assert.deepEqual([...storage.map.keys()], [SETTINGS_KEY]);
  const stored = JSON.parse(storage.map.get(SETTINGS_KEY));
  assert.equal(stored.version, SETTINGS_VERSION);
  assert.deepEqual(loadSettings(storage), { ...settings, version: SETTINGS_VERSION });
});

test("settings migrate the legacy quality and pointer-mode keys on first load", () => {
  assert.deepEqual(loadSettings(memoryStorage({ bp_quality: "balanced", bp_pointer_mode: "game" })),
    { ...DEFAULT_SETTINGS, quality: "balanced", pointerMode: "game" });
  // Once the record exists the legacy keys are ignored.
  const storage = memoryStorage({ bp_quality: "saver" });
  saveSettings(storage, { ...DEFAULT_SETTINGS, quality: "auto" });
  assert.equal(loadSettings(storage).quality, "auto");
});

test("settings fall back to defaults for corrupt, future or throwing storage", () => {
  assert.deepEqual(loadSettings(memoryStorage({ [SETTINGS_KEY]: "{not json" })), DEFAULT_SETTINGS);
  assert.deepEqual(loadSettings(memoryStorage({ [SETTINGS_KEY]: JSON.stringify({ version: 99, quality: "saver" }) })), DEFAULT_SETTINGS);
  assert.deepEqual(loadSettings(throwingStorage), DEFAULT_SETTINGS);
  assert.deepEqual(loadSettings(null), DEFAULT_SETTINGS);
  assert.equal(saveSettings(throwingStorage, DEFAULT_SETTINGS), false);
  assert.equal(saveSettings(null, DEFAULT_SETTINGS), false);
});

test("settings normalization rejects out-of-range values field by field", () => {
  assert.deepEqual(normalizeSettings({ quality: "ultra", pointerMode: "tablet", keyboardLock: "yes", scrollSpeed: 7, defaultMonitor: 3 }), DEFAULT_SETTINGS);
  assert.equal(normalizeSettings({ scrollSpeed: "0.5" }).scrollSpeed, 0.5);
  assert.equal(normalizeSettings({ defaultMonitor: "x".repeat(200) }).defaultMonitor, "");
  assert.equal(normalizeSettings({ followWindow: "true" }).followWindow, false);
});

test("follow active window is a boolean setting, off by default, added without resetting older records", () => {
  assert.equal(DEFAULT_SETTINGS.followWindow, false);
  // A version-1 record saved before the field existed keeps every other choice.
  const older = { version: SETTINGS_VERSION, quality: "saver", pointerMode: "game", keyboardLock: false, scrollSpeed: 2, defaultMonitor: "\\\\.\\DISPLAY2" };
  const storage = memoryStorage({ [SETTINGS_KEY]: JSON.stringify(older) });
  assert.deepEqual(loadSettings(storage), { ...older, followWindow: false, clipboardAutoCopy: true });
  saveSettings(storage, { ...loadSettings(storage), followWindow: true });
  assert.deepEqual(loadSettings(storage), { ...older, followWindow: true, clipboardAutoCopy: true });
});

test("automatic PC clipboard copying defaults on and persists an explicit opt-out", () => {
  assert.equal(DEFAULT_SETTINGS.clipboardAutoCopy, true);
  assert.equal(normalizeSettings({ clipboardAutoCopy: "false" }).clipboardAutoCopy, true);
  const storage = memoryStorage({ [SETTINGS_KEY]: JSON.stringify({ version: SETTINGS_VERSION, quality: "saver" }) });
  assert.equal(loadSettings(storage).clipboardAutoCopy, true);
  saveSettings(storage, { ...loadSettings(storage), clipboardAutoCopy: false });
  assert.equal(loadSettings(storage).clipboardAutoCopy, false);
  assert.equal(loadSettings(storage).quality, "saver");
  const html = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(html, /Copy from the PC automatically[\s\S]*?id="setting-clipboard-auto-copy" class="switch" type="checkbox" role="switch"/);
});

test("the Monitors popover has a Follow active window switch, hidden until the host announces it", () => {
  const html = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  const css = readFileSync(new URL("../public/bubbleplay.css", import.meta.url), "utf8");
  const popover = html.match(/<div id="pop-monitors"[\s\S]*?<\/button>\s*<\/div>\s*<\/div>/)?.[0] ?? "";
  // Below the list, behind a separator, hidden by default.
  assert.match(popover, /id="monitor-list"[\s\S]*<div id="follow-window-group" hidden>\s*<div class="pop-divider" role="separator"><\/div>/);
  // A button row (popover ArrowUp/Down and mousedown focus handling apply to buttons).
  assert.match(popover, /<button id="follow-window" class="pop-row follow-row" type="button" role="switch" aria-checked="false">/);
  assert.match(popover, /<span class="pop-row-title">Follow active window<\/span>/);
  assert.match(css, /\.follow-row\[aria-checked="true"\] \.pop-switch \{/);
});

test("the quality preset still flows through transport parsing from the settings record", () => {
  const params = new URLSearchParams();
  assert.equal(parseQualityPreset(params, qualityStorageView({ quality: "saver" })), "saver");
  assert.equal(parseQualityPreset(new URLSearchParams("quality=balanced"), qualityStorageView({ quality: "saver" })), "balanced");
});

test("shortcut chords press in order and release in reverse as physical keys", () => {
  assert.deepEqual(chordSteps(["ControlLeft", "ShiftLeft", "KeyT"]), [
    ["ControlLeft", false], ["ShiftLeft", false], ["KeyT", false],
    ["KeyT", true], ["ShiftLeft", true], ["ControlLeft", true],
  ]);
  assert.deepEqual(chordSteps(["MetaLeft"]), [["MetaLeft", false], ["MetaLeft", true]]);
  assert.equal(chordLabel(["ControlLeft", "ShiftLeft", "Escape"]), "Ctrl+Shift+Esc");
  assert.equal(keyName("AltLeft"), "Alt");
  assert.equal(keyName("F4"), "F4");
});

test("desktop shortcuts omit editing chords a physical keyboard already types", () => {
  const desktop = shortcutsFor("desktop").map(shortcut => shortcut.id);
  const touch = shortcutsFor("touch").map(shortcut => shortcut.id);
  assert.ok(!desktop.includes("copy") && touch.includes("copy"));
  for (const id of ["close-tab", "switch-window", "start", "show-desktop", "task-manager"]) assert.ok(desktop.includes(id), id);
  assert.equal(touch.length, SHORTCUTS.length);
  // Windows rejects an injected secure attention sequence.
  assert.ok(!SHORTCUTS.some(shortcut => shortcut.keys.includes("Delete") && shortcut.keys.includes("AltLeft")));
});

test("fullscreen edge reveal arms only after the pointer rests at the top edge", () => {
  const timers = [];
  let armedCalls = 0;
  const reveal = createEdgeReveal({
    delayMs: 300, edgePx: 4,
    schedule: (fn, ms) => { timers.push({ fn, ms, live: true }); return timers.length - 1; },
    cancel: id => { timers[id].live = false; },
    onArm: () => { armedCalls++; },
  });
  reveal.move(2, true);
  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 300);
  reveal.move(1, true); // still resting: no second timer
  assert.equal(timers.length, 1);
  reveal.move(40, true); // moved away: cancelled
  assert.equal(timers[0].live, false);
  reveal.move(0, false); // disabled (not fullscreen): nothing scheduled
  assert.equal(timers.length, 1);
  reveal.move(0, true);
  timers[1].fn();
  assert.equal(reveal.armed, true);
  assert.equal(armedCalls, 1);
  reveal.move(0, true); // already armed
  assert.equal(timers.length, 2);
  reveal.reset();
  assert.equal(reveal.armed, false);
});

test("popovers anchor below the desktop strip and above the touch bar inside the viewport", () => {
  const viewport = { width: 1024, height: 640 };
  const below = popoverPosition({ anchor: { left: 980, width: 32, top: 0, bottom: 32 }, popover: { width: 280, height: 300 }, viewport, placement: "below" });
  assert.equal(below.left, 1024 - 280 - 8);
  assert.equal(below.top, 38);
  assert.equal(below.maxHeight, 640 - 38 - 8);
  const above = popoverPosition({ anchor: { left: 0, width: 48, top: 795, bottom: 844 }, popover: { width: 300, height: 200 }, viewport: { width: 390, height: 844 }, placement: "above" });
  assert.equal(above.left, 8);
  assert.equal(above.bottom, 844 - 795 + 6);
  assert.equal(above.top, undefined);
});

test("file transfer lives in the Files popover with a picker, gated on the capability", () => {
  const html = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  const client = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  assert.match(html, /<button id="strip-files"[^>]*hidden/);
  assert.match(html, /<div id="pop-files"[\s\S]*<section id="file-transfer"[\s\S]*id="file-choose"[\s\S]*<input id="file-picker" type="file" multiple/);
  assert.match(html, /id="file-drop-veil"/);
  assert.match(client, /stripFiles\.hidden = !capable/);
  assert.match(client, /if \(files\.length && fileTransferAvailable\(\)\) fileClient\.upload\(files, "save"\)/);
  // Drop and paste uploads stay wired through file-ui.js and the paste hook.
  assert.match(client, /const pastedFiles = fileTransferAvailable\(\) \? filesFromPaste\(e\) : \[\]/);
  assert.match(client, /veil: document\.getElementById\("file-drop-veil"\)/);
});
