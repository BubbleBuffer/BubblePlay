import { annexBHasIdr, createLatestRenderSlot, createWorkerTimeline, decodeTimestampPatternBits, normalizeEncodedTimestamp } from "/latency.js";
import { transitionKeyframeVerdict } from "/monitor-selection.js";

let transformer, canvas, ctx, decoder, renderTimer = 0;
let waitingForKeyframe = true, decodePending = 0, lastKeyframeRequest = -Infinity;
let baseTimestamp = null, baseWall = 0, lastFpsWall = performance.now(), fpsFrames = 0;
// WebRTC transition gate from the main thread: drop frames until a keyframe at
// or after the admitted IDR's RTP timestamp, then count draws for its token.
let transitionGate = null;
let renderedDue = null, diagnostics = false, lastStatsAt = 0, lastBarcodeAt = 0, barcodeValue = null, barcodeSequence = null;
const MAX_DECODE_QUEUE_SIZE = 8;
const MAX_PACING_DELAY_MS = 0;
const timeline = createWorkerTimeline({ now: () => performance.now() });
const renderSlot = createLatestRenderSlot();
const stats = { renderedTimestamp: null, renderedAgeMs: null, framesDrawn: 0, framesReceived: 0, transformFrames: 0, keyframes: 0, renderReplacements: 0, fps: 0, dropped: 0, resyncs: 0, lastResetReason: null, recovering: true, glassToGlass: null, transitionToken: null, transitionDrawn: 0, transitionPassed: false };

function control(type, extra = {}, force = true) { const now = performance.now(), interval = diagnostics ? 50 : 250; if (!force && now - lastStatsAt < interval) return; lastStatsAt = now; const live = { ...stats, decodePending, decodeQueueSize: decoder?.decodeQueueSize || 0, waitingForKeyframe, codec: pendingCodec, renderedAgeMs: renderedDue === null ? null : Math.max(0, now - renderedDue) }; if (barcodeValue) { const ageMs = ((Date.now() >>> 0) - barcodeValue.timestamp) >>> 0; live.glassToGlass = ageMs < 10_000 ? { ...barcodeValue, ageMs } : null; } self.postMessage({ kind: "control", type, stats: live, ...extra }); }
function requestKeyframe(reason) {
  if (!transformer || performance.now() - lastKeyframeRequest < 500) return;
  lastKeyframeRequest = performance.now();
  Promise.resolve(transformer.sendKeyFrameRequest()).then(() => control("keyframe-requested", { reason })).catch((error) => control("keyframe-request-error", { message: String(error) }));
}
function clearRender() { if (renderTimer) clearTimeout(renderTimer); renderTimer = 0; const pending = renderSlot.clear(); if (pending) { try { pending.frame.close(); } catch { /* already closed */ } } }
function reset(reason) {
  stats.resyncs++; stats.lastResetReason = reason; stats.recovering = true; decodePending = 0; waitingForKeyframe = true; baseTimestamp = null; timeline.reset(); clearRender();
  if (decoder) { try { decoder.close(); } catch { /* already closed */ } decoder = null; }
  control("resync", { reason }); requestKeyframe(reason);
}
function barcode() {
  if (!ctx || canvas.width < 832 || canvas.height < 112) return null;
  try { const image = ctx.getImageData(64, 64, 768, 48), bits = [];
    for (let i = 0; i < 64; i++) { const x = (i % 32) * 24 + 12, y = Math.floor(i / 32) * 24 + 12, p = (y * image.width + x) * 4; bits.push(image.data[p] + image.data[p + 1] + image.data[p + 2] >= 384 ? 1 : 0); }
    const decoded = decodeTimestampPatternBits(bits); if (!decoded || decoded.sequence === barcodeSequence) return null;
    barcodeSequence = decoded.sequence; return decoded;
  } catch { return null; }
}
function render() {
  renderTimer = 0; const item = renderSlot.take(); if (!item) return;
  const age = performance.now() - item.due;
  if (age > 100) { try { item.frame.close(); } finally { stats.dropped++; reset("render-late"); } return; }
  try { if (canvas.width !== item.frame.displayWidth || canvas.height !== item.frame.displayHeight) { canvas.width = item.frame.displayWidth; canvas.height = item.frame.displayHeight; }
    ctx.drawImage(item.frame, 0, 0); stats.renderedTimestamp = item.frame.timestamp; renderedDue = item.due; stats.framesDrawn++; if (stats.transitionPassed) stats.transitionDrawn++; stats.recovering = false; fpsFrames++;
    if (performance.now() - lastFpsWall >= 1000) { stats.fps = fpsFrames; fpsFrames = 0; lastFpsWall = performance.now(); }
    const now = performance.now(); if (diagnostics && now - lastBarcodeAt >= 100) { lastBarcodeAt = now; barcodeValue = barcode() || barcodeValue; } control("worker-stats", {}, false);
  } finally { try { item.frame.close(); } catch { /* already closed */ } }
}
function enqueue(frame) {
  const now = performance.now(); if (baseTimestamp === null) { baseTimestamp = frame.timestamp; baseWall = now; }
  const due = baseWall + (frame.timestamp - baseTimestamp) / 1000;
  const { arm, replaced } = renderSlot.offer({ frame, due });
  if (replaced) { try { replaced.frame.close(); } finally { stats.dropped++; stats.renderReplacements++; } }
  // Latest-frame-wins is the latency policy. A zero-delay Worker task lets a
  // same-turn decode burst collapse into one draw without adding a frame of
  // intentional playout delay.
  if (arm) renderTimer = setTimeout(render, Math.min(MAX_PACING_DELAY_MS, Math.max(0, due - now)));
}
function ensureDecoder() {
  if (decoder) return true;
  try { decoder = new VideoDecoder({ output: (frame) => { decodePending = Math.max(0, decodePending - 1); stats.framesReceived++; enqueue(frame); }, error: () => reset("decoder-error") }); decoder.configure({ codec: pendingCodec || "avc1.64002A", optimizeForLatency: true, hardwareAcceleration: "prefer-hardware" }); return true; }
  catch { reset("decoder-configure"); return false; }
}
let pendingCodec = null;
function codecFromSps(bytes) { for (let i = 0; i + 7 < bytes.length; i++) { const sc3 = bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1, sc4 = bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 0 && bytes[i + 3] === 1; const at = i + (sc4 ? 4 : 3); if ((sc3 || sc4) && (bytes[at] & 31) === 7) return `avc1.${[bytes[at + 1], bytes[at + 2], bytes[at + 3]].map((n) => n.toString(16).padStart(2, "0")).join("")}`; } return null; }
function decode(value) {
  stats.transformFrames++;
  const metadata = typeof value.getMetadata === "function" ? value.getMetadata() : {};
  const type = value.type === "key" || annexBHasIdr(new Uint8Array(value.data)) ? "key" : "delta";
  if (type === "key") stats.keyframes++;
  const timestamp = normalizeEncodedTimestamp(value.timestamp, metadata), observed = timeline.observe(timestamp);
  if (diagnostics) stats.inputTiming = { valueTimestamp: value.timestamp, pts: metadata.timestamp,
    rtpTimestamp: metadata.rtpTimestamp, normalizedUs: timestamp, receivedAtMs: performance.now() };
  if (!observed.ok) { reset(observed.reason); return; }
  if (waitingForKeyframe && type !== "key") { stats.dropped++; requestKeyframe("startup-delta"); return; }
  if (transitionGate) {
    // After an unprovable keyframe the main thread restarts the peer; keep
    // dropping until then.
    if (transitionGate.unprovable) { stats.dropped++; return; }
    const verdict = transitionKeyframeVerdict({ isKey: type === "key", frameRtpTimestamp: metadata.rtpTimestamp, admittedRtpTimestamp: transitionGate.admittedRtpTimestamp, strict: transitionGate.strict });
    if (verdict === "unprovable") { transitionGate.unprovable = true; stats.dropped++; control("transition-unprovable", { token: stats.transitionToken }); return; }
    if (verdict !== "admit") { stats.dropped++; requestKeyframe("transition-keyframe"); return; }
    transitionGate = null; stats.transitionPassed = true;
  }
  if (type === "key") pendingCodec = codecFromSps(new Uint8Array(value.data)) || pendingCodec;
  if (!ensureDecoder()) return;
  // decodePending includes frames already accepted by Android's hardware
  // decoder. Those are not JS-side backlog. WebCodecs' decodeQueueSize is the
  // actual control queue we must bound to prevent latency accumulation.
  if (decoder.decodeQueueSize >= MAX_DECODE_QUEUE_SIZE) { stats.dropped++; reset("decode-queue-overflow"); return; }
  waitingForKeyframe = false; decodePending++;
  try { decoder.decode(new EncodedVideoChunk({ type, timestamp, data: value.data })); }
  catch { decodePending--; reset("decode-throw"); }
}
self.onmessage = ({ data }) => {
  if (data.type === "capabilities") { let supported = false; try { supported = typeof VideoDecoder !== "undefined" && typeof OffscreenCanvas !== "undefined" && !!new OffscreenCanvas(1, 1).getContext("2d"); } catch { /* the probe throwing means unsupported */ } control("capabilities", { supported }); }
  else if (data.type === "canvas") { canvas = data.canvas; diagnostics = !!data.diagnostics; ctx = canvas.getContext("2d", { alpha: false, desynchronized: true }); if (ctx) control("worker-ready"); else control("worker-init-error", { message: "OffscreenCanvas 2D context unavailable" }); }
  else if (data.type === "resync") {
    if (data.transition && typeof data.transition.token === "string") {
      transitionGate = { admittedRtpTimestamp: data.transition.admittedRtpTimestamp ?? null, strict: data.transition.strict === true, unprovable: false };
      stats.transitionToken = data.transition.token; stats.transitionDrawn = 0; stats.transitionPassed = false;
    }
    reset(data.reason || "main-resync");
  }
  else if (data.type === "request-keyframe") requestKeyframe(data.reason);
};
self.onrtctransform = ({ transformer: incoming }) => {
  transformer = incoming; const reader = transformer.readable.getReader(), writer = transformer.writable.getWriter();
  (async () => { for (;;) { const { value, done } = await reader.read(); if (done) break; if (!value) continue; if (canvas) decode(value); await writer.write(value); } })().catch((error) => control("worker-error", { message: String(error) }));
};
