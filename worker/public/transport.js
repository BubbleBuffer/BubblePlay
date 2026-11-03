export const TRANSPORT = Object.freeze({ AUTO: "auto", WEBRTC: "webrtc", WSS: "wss" });
export const QUALITY = Object.freeze({ AUTO: "auto", HIGH: "high", BALANCED: "balanced", SAVER: "saver" });

export function rtpCodecMimeTypes(codec) {
  return codec === "hevc" ? ["video/h265", "video/hevc"] : codec === "h264" ? ["video/h264"] : [];
}

// HEVC is followed by H.264 so an H.264-only host can still answer a
// HEVC-preferring viewer. H.264 never pulls in HEVC.
const RTP_PREFERENCE_FAMILIES = Object.freeze({ hevc: ["hevc", "h264"], h264: ["h264"] });

/** Orders receiver capabilities by family, ignoring getCapabilities order. Capabilities keep their relative order within a family. */
export function orderRtpCodecPreferences(capabilities, codec) {
  const ordered = [];
  for (const family of RTP_PREFERENCE_FAMILIES[codec] ?? []) {
    const mimeTypes = rtpCodecMimeTypes(family);
    for (const capability of capabilities ?? []) {
      if (mimeTypes.includes(String(capability?.mimeType ?? "").toLowerCase())) ordered.push(capability);
    }
  }
  return ordered;
}

/** Query-only selection; it deliberately never reads or writes persistent storage. */
export function parseTransport(params) {
  if (params.get("wsl") === "1" || params.get("transport") === "wsl") return TRANSPORT.WSS;
  const value = params.get("transport");
  return Object.values(TRANSPORT).includes(value) ? value : TRANSPORT.AUTO;
}

// Auto mode remembers fallback only within this tab's session. Never consult
// the legacy localStorage record: a future visit deserves a fresh RTC attempt.
export const RTC_FAILURE_STORAGE_KEY = "bp_rtc_failed_at";

export function rtcFailureStorage(browser = globalThis.window) {
  try { return browser?.sessionStorage ?? null; } catch { return null; }
}

/** True when an auto-mode page should skip WebRTC and start on WSS. */
export function rememberedRtcFailure(requested, storage, nowMs = Date.now()) {
  if (requested !== TRANSPORT.AUTO) return false;
  let failedAt = NaN;
  try { failedAt = Number(storage?.getItem(RTC_FAILURE_STORAGE_KEY)); } catch { return false; }
  return Number.isFinite(failedAt) && failedAt > 0 && nowMs >= failedAt;
}

export function rememberRtcFailure(storage, nowMs = Date.now()) {
  try { storage?.setItem(RTC_FAILURE_STORAGE_KEY, String(Math.floor(nowMs))); } catch { /* private mode */ }
}

export function forgetRtcFailure(storage) {
  try { storage?.removeItem(RTC_FAILURE_STORAGE_KEY); } catch { /* private mode */ }
}

export function parseQualityPreset(params, storage = null) {
  const requested = params.get("quality");
  if (Object.values(QUALITY).includes(requested)) return requested;
  try {
    const remembered = storage?.getItem("bp_quality");
    if (Object.values(QUALITY).includes(remembered)) return remembered;
  } catch { /* storage may be unavailable in private contexts */ }
  return QUALITY.HIGH;
}

export function decoderSupportsCodec(transport, decoderPath, codec) {
  if (!["hevc", "h264"].includes(codec)) return false;
  return transport === "wss" || decoderPath !== "webcodecs" || codec === "h264";
}

/** Codec selection is page-local and constrained by the selected decoder. */
export function createCodecSession({ transport = "webrtc", decoderPath = "native" } = {}) {
  let codec = decoderSupportsCodec(transport, decoderPath, "hevc") ? "hevc" : "h264";
  let h264FallbackUsed = codec === "h264";
  return {
    get codec() { return codec; },
    get h264FallbackUsed() { return h264FallbackUsed; },
    followBitstreamCodec(nextCodec) {
      if (!["hevc", "h264"].includes(nextCodec) || nextCodec === codec) return false;
      codec = nextCodec;
      return true;
    },
    fallbackToH264() {
      if (h264FallbackUsed) return false;
      h264FallbackUsed = true;
      codec = "h264";
      return true;
    },
  };
}

/**
 * Where WSS video is decoded and drawn: "worker" (WebCodecs plus OffscreenCanvas in
 * a Dedicated Worker) or "main" (today's main-thread path). `?decoderPath=main`
 * forces main for debugging, `?decoderPath=worker` forces the worker when the
 * browser can do it. WebRTC does not use this: its default stays native.
 */
export const WSS_WORKER_DEFAULT = false;
export function selectWssDecoderPath({ requested = null, supported = false, disabled = false, workerDefault = WSS_WORKER_DEFAULT } = {}) {
  if (disabled || !supported || requested === "main") return "main";
  return requested === "worker" || workerDefault ? "worker" : "main";
}
