export const TRANSPORT = Object.freeze({ AUTO: "auto", WEBRTC: "webrtc", WSS: "wss" });
export const QUALITY = Object.freeze({ AUTO: "auto", HIGH: "high", BALANCED: "balanced", SAVER: "saver" });

export function rtpCodecMimeTypes(codec) {
  return codec === "hevc" ? ["video/h265", "video/hevc"] : codec === "h264" ? ["video/h264"] : [];
}

/** Query-only selection; it deliberately never reads or writes persistent storage. */
export function parseTransport(params) {
  if (params.get("wsl") === "1" || params.get("transport") === "wsl") return TRANSPORT.WSS;
  const value = params.get("transport");
  return Object.values(TRANSPORT).includes(value) ? value : TRANSPORT.AUTO;
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

export function parseAdaptive(params) {
  if (params.get("adaptive") === "0") return false;
  if (params.get("adaptive") === "1") return true;
  return params.get("quality") === QUALITY.AUTO;
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
    fallbackToH264() {
      if (h264FallbackUsed) return false;
      h264FallbackUsed = true;
      codec = "h264";
      return true;
    },
  };
}
