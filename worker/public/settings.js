// Receiver settings: one namespaced, versioned localStorage record.
// Every read and write tolerates a missing or throwing storage (private
// windows, blocked site data); the page then runs on defaults.

export const SETTINGS_KEY = "bubbleplay.settings";
export const SETTINGS_VERSION = 1;
export const QUALITY_PRESETS = Object.freeze(["auto", "high", "balanced", "saver"]);
export const SCROLL_SPEEDS = Object.freeze([0.5, 1, 1.5, 2]);
export const POINTER_MODES = Object.freeze(["desktop", "game"]);
export const DEFAULT_SOURCE_URL = "https://github.com/BubbleBuffer/BubblePlay";

export function sourceURL(value) {
  try {
    const url = new URL(value);
    if (["https:", "http:"].includes(url.protocol) && !url.username && !url.password) return url.href;
  } catch { /* missing or invalid: retain the default */ }
  return DEFAULT_SOURCE_URL;
}

export const DEFAULT_SETTINGS = Object.freeze({
  version: SETTINGS_VERSION,
  quality: "high",
  pointerMode: "desktop",
  keyboardLock: true,
  clipboardAutoCopy: true,
  scrollSpeed: 1,
  defaultMonitor: "",
  // Added within version 1: older records simply lack it and read as false.
  followWindow: false,
  // PC id -> "desktop" | "game". A PC without an entry uses pointerMode.
  pointerModes: Object.freeze({}),
});

// Keys written by receivers before the settings record existed.
const LEGACY_KEYS = Object.freeze({ quality: "bp_quality", pointerMode: "bp_pointer_mode" });

function safeGet(storage, key) {
  try { return storage?.getItem(key) ?? null; } catch { return null; }
}

const MAX_PC_MODES = 64;
const MAX_PC_ID_LENGTH = 128;

function normalizePointerModes(raw) {
  const modes = {};
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    for (const [pc, mode] of Object.entries(raw)) {
      if (Object.keys(modes).length >= MAX_PC_MODES) break;
      if (pc.length > 0 && pc.length <= MAX_PC_ID_LENGTH && pc !== "__proto__" && POINTER_MODES.includes(mode)) modes[pc] = mode;
    }
  }
  return modes;
}

/** The pointer mode for one PC: its own entry, else the global value. */
export function pointerModeFor(settings, pc) {
  const own = typeof pc === "string" && Object.hasOwn(settings.pointerModes ?? {}, pc) ? settings.pointerModes[pc] : null;
  return POINTER_MODES.includes(own) ? own : (settings.pointerMode === "game" ? "game" : "desktop");
}

/** Records a mode for one PC; without a PC id it sets the global value. */
export function rememberPointerMode(settings, pc, mode) {
  if (!POINTER_MODES.includes(mode)) return;
  if (typeof pc === "string" && pc && pc !== "__proto__") {
    const modes = { ...normalizePointerModes(settings.pointerModes) };
    delete modes[pc];
    modes[pc] = mode;
    // Keep the most recently changed PCs when the cap is reached.
    const keep = Object.entries(modes).slice(-MAX_PC_MODES);
    settings.pointerModes = Object.fromEntries(keep);
  } else settings.pointerMode = mode;
}

/** Coerces any stored value into a complete, valid settings record. */
export function normalizeSettings(raw) {
  const value = raw && typeof raw === "object" ? raw : {};
  return {
    version: SETTINGS_VERSION,
    quality: QUALITY_PRESETS.includes(value.quality) ? value.quality : DEFAULT_SETTINGS.quality,
    pointerMode: POINTER_MODES.includes(value.pointerMode) ? value.pointerMode : DEFAULT_SETTINGS.pointerMode,
    keyboardLock: typeof value.keyboardLock === "boolean" ? value.keyboardLock : DEFAULT_SETTINGS.keyboardLock,
    clipboardAutoCopy: typeof value.clipboardAutoCopy === "boolean" ? value.clipboardAutoCopy : DEFAULT_SETTINGS.clipboardAutoCopy,
    scrollSpeed: SCROLL_SPEEDS.includes(Number(value.scrollSpeed)) ? Number(value.scrollSpeed) : DEFAULT_SETTINGS.scrollSpeed,
    defaultMonitor: typeof value.defaultMonitor === "string" && value.defaultMonitor.length <= 128 ? value.defaultMonitor : "",
    followWindow: typeof value.followWindow === "boolean" ? value.followWindow : DEFAULT_SETTINGS.followWindow,
    pointerModes: normalizePointerModes(value.pointerModes),
  };
}

/** Reads the record; a first visit migrates the legacy per-setting keys. */
export function loadSettings(storage) {
  const stored = safeGet(storage, SETTINGS_KEY);
  if (stored !== null) {
    try {
      const parsed = JSON.parse(stored);
      // A newer record shape is never half-read: fall back to defaults.
      if (parsed?.version === SETTINGS_VERSION) return normalizeSettings(parsed);
    } catch { /* corrupt record: defaults */ }
    return normalizeSettings(null);
  }
  const legacyMode = safeGet(storage, LEGACY_KEYS.pointerMode);
  return normalizeSettings({
    quality: safeGet(storage, LEGACY_KEYS.quality),
    pointerMode: legacyMode === "game" ? "game" : "desktop",
  });
}

/** Writes the whole record. Returns false when storage refused it. */
export function saveSettings(storage, settings) {
  try {
    storage?.setItem(SETTINGS_KEY, JSON.stringify(normalizeSettings(settings)));
    return Boolean(storage);
  } catch { return false; }
}

/**
 * A storage view for parseQualityPreset (transport.js), which reads the
 * legacy "bp_quality" key: answer it from the settings record instead.
 */
export function qualityStorageView(settings) {
  return { getItem: key => key === LEGACY_KEYS.quality ? settings.quality : null };
}
