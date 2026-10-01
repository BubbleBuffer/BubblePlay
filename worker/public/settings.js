// Receiver settings: one namespaced, versioned localStorage record.
// Every read and write tolerates a missing or throwing storage (private
// windows, blocked site data); the page then runs on defaults.

export const SETTINGS_KEY = "bubbleplay.settings";
export const SETTINGS_VERSION = 1;
export const QUALITY_PRESETS = Object.freeze(["auto", "high", "balanced", "saver"]);
export const SCROLL_SPEEDS = Object.freeze([0.5, 1, 1.5, 2]);
export const POINTER_MODES = Object.freeze(["desktop", "game"]);

export const DEFAULT_SETTINGS = Object.freeze({
  version: SETTINGS_VERSION,
  quality: "high",
  pointerMode: "desktop",
  keyboardLock: true,
  scrollSpeed: 1,
  defaultMonitor: "",
});

// Keys written by receivers before the settings record existed.
const LEGACY_KEYS = Object.freeze({ quality: "bp_quality", pointerMode: "bp_pointer_mode" });

function safeGet(storage, key) {
  try { return storage?.getItem(key) ?? null; } catch { return null; }
}

/** Coerces any stored value into a complete, valid settings record. */
export function normalizeSettings(raw) {
  const value = raw && typeof raw === "object" ? raw : {};
  return {
    version: SETTINGS_VERSION,
    quality: QUALITY_PRESETS.includes(value.quality) ? value.quality : DEFAULT_SETTINGS.quality,
    pointerMode: POINTER_MODES.includes(value.pointerMode) ? value.pointerMode : DEFAULT_SETTINGS.pointerMode,
    keyboardLock: typeof value.keyboardLock === "boolean" ? value.keyboardLock : DEFAULT_SETTINGS.keyboardLock,
    scrollSpeed: SCROLL_SPEEDS.includes(Number(value.scrollSpeed)) ? Number(value.scrollSpeed) : DEFAULT_SETTINGS.scrollSpeed,
    defaultMonitor: typeof value.defaultMonitor === "string" && value.defaultMonitor.length <= 128 ? value.defaultMonitor : "",
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
