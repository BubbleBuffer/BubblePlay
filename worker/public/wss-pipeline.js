// WSS video pipeline that runs inside a Dedicated Worker: WebCodecs decode plus
// every canvas draw, so the main thread keeps only layout, input, HUD and the
// cursor overlay. It is a port of the main-thread path in receiver.js
// (decodeWssFrame / feedDecoder / onDecodedFrame / drawFrame) with the same
// policies: optimizeForLatency, immediate draw with no rAF, desynchronized
// canvas, deltas dropped until a keyframe, and decode of every frame with only
// presentation skipped while the decoder is behind.
//
// DOM-free on purpose: the decoder, chunk factory, canvases and clock are
// injected, so worker/test can drive it with fakes. All results go out through
// `post(message)`; the main thread (wss-worker-client.js + receiver.js) owns
// recovery state, the HUD and ACKs (ACKs stay on receipt there).
import { detectAnnexBCodec, shouldPresentDecodedV4 } from "./media-v4.js";
import { CODEC, drawBaseThenHybrid, HevcTileCache, HybridTileCache, prepareHybridWssFrame } from "./media.js";

// Parse the codec string (avc1.PPCCLL) from the first SPS NAL of an Annex B
// keyframe. profile_idc/constraint/level are the first 3 bytes of the SPS
// RBSP and never contain emulation-prevention bytes.
export function avcCodecFromSps(bytes) {
  const hex2 = (n) => n.toString(16).toUpperCase().padStart(2, "0");
  let i = 0;
  while (i + 4 < bytes.length) {
    const sc = bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1;
    const sc4 = bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 0 && bytes[i + 3] === 1;
    if (!sc && !sc4) { i++; continue; }
    const nalStart = i + (sc4 ? 4 : 3);
    const nalType = bytes[nalStart] & 0x1f;
    if (nalType === 7 && nalStart + 4 < bytes.length) {
      return `avc1.${hex2(bytes[nalStart + 1])}${hex2(bytes[nalStart + 2])}${hex2(bytes[nalStart + 3])}`;
    }
    i = nalStart;
  }
  return null;
}

const DEFAULT_HEVC_CODEC = "hev1.1.6.L120.B0";
const MAX_PENDING_TIMESTAMPS = 64;
const HYBRID_MODES = ["png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles"];
const HYBRID_SNAPSHOT_INTERVAL_MS = 250;

export function createWssPipeline({
  post,
  now = () => performance.now(),
  createDecoder = (init) => new VideoDecoder(init),
  createChunk = (init) => new EncodedVideoChunk(init),
  hybridTileCache = new HybridTileCache(),
  hevcTileCache = new HevcTileCache(),
} = {}) {
  let main = null; // { canvas, ctx }: the full-desktop canvas
  let sharp = null; // { canvas, ctx }: the sharp-viewport crop overlay
  let epoch = 0;
  let codec = "hevc";
  let decoder = null;
  let sawKeyframe = false;
  let desktopAvailable = true;
  let lastPresentAt = -Infinity;
  let seqByTimestamp = new Map();
  let frameMeta = new WeakMap();
  let firstConfigured = false;
  let firstOutput = false;
  let timestampFallbackLogged = false;
  let hybridMode = "off";
  let lastHybridSnapshotAt = -Infinity;

  const log = (level, name, fields = {}) => post({ type: "log", level, name, fields });
  const trace = (name, fields = {}) => post({ type: "trace", name, fields });
  const fail = (reason, hud = null) => post({ type: "failure", reason, hud });

  function attach(canvases) {
    const context = (canvas) => canvas.getContext("2d", { alpha: false, desynchronized: true });
    const ctx = context(canvases.main);
    if (!ctx) return false;
    main = { canvas: canvases.main, ctx };
    // The crop overlay is its own canvas so the base picture stays in place
    // while a zoomed region is shown at full resolution on top of it.
    if (canvases.sharp) {
      const sharpCtx = context(canvases.sharp);
      if (!sharpCtx) return false;
      sharp = { canvas: canvases.sharp, ctx: sharpCtx };
    }
    return true;
  }

  // Hybrid tile overlays only exist for HEVC; the mode name comes from the
  // host's bench status and is sent along with every frame.
  const hybridActive = () => codec === "hevc" && HYBRID_MODES.includes(hybridMode);
  const activeCache = () => (hybridMode === "hevc-quiescent-tiles" ? hevcTileCache : hybridTileCache);
  function clearCaches() {
    hybridTileCache.clear();
    hevcTileCache.clear();
  }

  function closeDecoder() {
    try { decoder?.close(); } catch { /* already closed */ }
    decoder = null;
    sawKeyframe = false;
  }

  function resetFrameState() {
    seqByTimestamp = new Map();
    frameMeta = new WeakMap();
  }

  function feed(chunk, hevcCodec) {
    const data = chunk.data;
    const isKey = chunk.type === "key";
    if (!decoder) {
      // WSS HEVC is Annex-B and has no decoder description/config record.
      const codecString = codec === "hevc" ? (hevcCodec || DEFAULT_HEVC_CODEC) : (avcCodecFromSps(data) || "avc1.64002A");
      sawKeyframe = false;
      let created = null;
      created = createDecoder({
        output: (frame) => onOutput(frame, created),
        error: (error) => onError(error, created),
      });
      decoder = created;
      try {
        decoder.configure({ codec: codecString, optimizeForLatency: true, hardwareAcceleration: "prefer-hardware" });
        post({ type: "decoder-configured", codec: codecString });
        if (!firstConfigured) { firstConfigured = true; trace("decoder-configured", { codec: codecString }); }
      } catch (error) {
        log("error", "decoder-config-error", { message: String(error?.message || error), codec: codecString });
        closeDecoder();
        fail("configure-unsupported", `cfg:${error?.message}`);
        return false;
      }
    }
    if (decoder.state !== "configured") return false;
    if (!isKey && !sawKeyframe) {
      post({ type: "drop", recover: "delta-before-keyframe" });
      return false;
    }
    sawKeyframe = sawKeyframe || isKey;
    try {
      if (chunk.frameTrace) chunk.frameTrace.decodeQueuedAtMs = now();
      decoder.decode(createChunk({ type: isKey ? "key" : "delta", timestamp: chunk.timestamp, data }));
      return true;
    } catch (error) {
      log("error", "decoder-decode-throw", {
        message: String(error?.message || error).slice(0, 512),
        codec: chunk.codec, type: isKey ? "key" : "delta", byteLength: data.byteLength,
      });
      fail("decode-throw", `dec:${error?.message}`);
      return false;
    }
  }

  function onError(error, owner) {
    if (owner !== decoder) return;
    log("error", "decoder-error", { message: String(error?.message || error) });
    fail("decode-error", `err:${error?.message}`);
  }

  function onOutput(frame, owner) {
    if (owner !== decoder) { try { frame.close(); } catch { /* ignore */ } return; }
    if (!firstOutput) { firstOutput = true; trace("decoder-first-output", { timestamp: frame.timestamp }); }
    let metadata = seqByTimestamp.get(frame.timestamp);
    if (metadata !== undefined) {
      seqByTimestamp.delete(frame.timestamp);
    } else if (seqByTimestamp.size) {
      // Some Chromium builds normalize WebCodecs output timestamps by a few
      // microseconds. WSS frames are decoded in order, so the oldest pending
      // metadata is the safe association fallback.
      const first = seqByTimestamp.entries().next().value;
      if (first) {
        metadata = first[1];
        seqByTimestamp.delete(first[0]);
        if (!timestampFallbackLogged) {
          timestampFallbackLogged = true;
          log("warn", "wss-timestamp-association-fallback", { frameTimestamp: frame.timestamp, metadataTimestamp: first[0] });
        }
      }
    }
    if (metadata !== undefined) {
      metadata.frameTrace.decodedAtMs = now();
      frameMeta.set(frame, metadata);
    }
    present(frame, metadata);
  }

  function present(frame, metadata) {
    const out = {
      type: "out",
      timestamp: frame.timestamp,
      decodedSequence: metadata ? metadata.sequence >>> 0 : null,
      key: metadata?.key === true,
      queued: decoder?.decodeQueueSize ?? 0,
      attempted: false,
      presented: false,
    };
    try {
      const t = now();
      if (!shouldPresentDecodedV4({ decodeQueueSize: decoder?.decodeQueueSize ?? 0, msSinceLastPresent: t - lastPresentAt })) return;
      lastPresentAt = t;
      out.attempted = true;
      draw(frame, metadata, out);
    } finally {
      try { frame.close(); } catch { /* ignore */ }
      post(out);
    }
  }

  function draw(frame, metadata, out) {
    if (!desktopAvailable) return;
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    out.width = w;
    out.height = h;
    const view = metadata?.view ?? null;
    // Never render a generation with stale geometry.
    if (!metadata || !view) return;
    const hybrid = hybridActive();
    const cache = activeCache();
    const composite = (context) => (hybrid
      ? drawBaseThenHybrid(context, frame, cache, metadata, w, h)
      : (context.drawImage(frame, 0, 0, w, h), 0));
    if (view.enabled && sharp) {
      if (sharp.canvas.width !== w || sharp.canvas.height !== h) {
        sharp.canvas.width = w;
        sharp.canvas.height = h;
      }
      const tiles = composite(sharp.ctx);
      out.sharp = true;
      finishPresent(frame, metadata, out, view, tiles);
      return;
    }
    if (main.canvas.width !== w || main.canvas.height !== h) {
      main.canvas.width = w;
      main.canvas.height = h;
      out.resized = true;
    }
    const tiles = composite(main.ctx);
    out.sharp = false;
    finishPresent(frame, metadata, out, view, tiles);
  }

  function finishPresent(frame, metadata, out, view, tilesDrawn) {
    const renderedAt = now();
    metadata.frameTrace.hybridTilesDrawn = tilesDrawn;
    metadata.frameTrace.presentedAtMs = renderedAt;
    out.presented = true;
    out.view = view;
    out.generation = metadata.generation;
    out.cropRevision = metadata.cropRevision;
    out.sequence = metadata.sequence >>> 0;
    out.renderedAt = renderedAt;
    out.trace = { ...metadata.frameTrace };
    if (hybridActive() && renderedAt - lastHybridSnapshotAt >= HYBRID_SNAPSHOT_INTERVAL_MS) {
      lastHybridSnapshotAt = renderedAt;
      out.hybrid = activeCache().snapshot();
    }
  }

  function handleFrame(message) {
    if (message.epoch !== epoch) return; // issued before the last resync
    desktopAvailable = message.desktopAvailable !== false;
    const data = message.data instanceof Uint8Array ? message.data
      : new Uint8Array(message.buffer, message.byteOffset || 0, message.byteLength);
    const key = message.key === true;
    const detectedCodec = detectAnnexBCodec(data, { key });
    const headerCodec = message.codec === CODEC.HEVC ? "hevc" : "h264";
    if (!key && (detectedCodec || headerCodec) !== codec) return;
    hybridMode = typeof message.hybridMode === "string" ? message.hybridMode : "off";
    const prepared = prepareHybridWssFrame({ generation: message.generation, sequence: message.sequence, data }, {
      active: hybridActive() && detectedCodec !== "h264",
      cache: activeCache(),
    });
    if (!prepared) { fail("hybrid-frame-invalid"); return; }
    const bitstreamCodec = detectedCodec || detectAnnexBCodec(prepared.data, { key });
    const frameCodec = bitstreamCodec || headerCodec;
    if (frameCodec !== codec) {
      if (!key || !bitstreamCodec) return;
      followCodec(frameCodec);
    }
    const activity = prepared.activity;
    if (activity && (activity.upserts || activity.invalidations || activity.clears || activity.pngBytes)) {
      log("info", "hybrid-frame-activity", {
        generation: message.generation, sequence: message.sequence, cropRevision: prepared.cropRevision,
        ...activity, hybrid: activeCache().snapshot(),
      });
    }
    const frameTrace = message.frameTrace ? { ...message.frameTrace } : {};
    if (activity) {
      frameTrace.hybridCropRevision = prepared.cropRevision;
      frameTrace.hybridTileUpserts = activity.upserts;
      frameTrace.hybridTileInvalidations = activity.invalidations;
      frameTrace.hybridTileSuppressions = activity.suppressions;
      frameTrace.hybridTileReactivations = activity.reactivations;
      frameTrace.hybridTilePermanentInvalidations = activity.permanentInvalidations;
      frameTrace.hybridTileClears = activity.clears;
      frameTrace.hybridPngBytes = activity.pngBytes;
    }
    if (key) post({ type: "key-accepted", generation: message.generation, sequence: message.sequence, codec: message.codec });
    const timestamp = message.timestampUs;
    seqByTimestamp.set(timestamp, {
      sequence: message.sequence,
      generation: message.generation,
      key,
      cropRevision: prepared.cropRevision,
      frameTrace,
      view: message.view ?? null,
    });
    while (seqByTimestamp.size > MAX_PENDING_TIMESTAMPS) seqByTimestamp.delete(seqByTimestamp.keys().next().value);
    const fed = feed({ type: key ? "key" : "delta", timestamp, data: prepared.data, codec, frameTrace }, message.hevcCodec);
    // Arm only after a configured decoder accepted the keyframe.
    if (fed && key && decoder?.state === "configured") post({ type: "key-fed", epoch });
  }

  function followCodec(next) {
    const previousCodec = codec;
    codec = next;
    closeDecoder();
    resetFrameState();
    clearCaches();
    post({ type: "codec-follow", previousCodec, codec: next });
  }

  function resync(message) {
    epoch = message.epoch;
    closeDecoder();
    resetFrameState();
    clearCaches();
  }

  function setTileCapability(capability) {
    hevcTileCache.setDecoderCapability(capability);
  }

  function setCodec(next) {
    if (next === "hevc" || next === "h264") codec = next;
  }

  function stop() {
    closeDecoder();
    resetFrameState();
    hybridTileCache.clear();
    hevcTileCache.close();
  }

  return {
    attach,
    handleFrame,
    resync,
    clearCaches,
    setTileCapability,
    setCodec,
    stop,
    get codec() { return codec; },
    get epoch() { return epoch; },
    get decoder() { return decoder; },
  };
}
