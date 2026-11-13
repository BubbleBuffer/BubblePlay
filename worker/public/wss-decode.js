// The WSS decode worker of one session, seen from the receiver: start it with the two canvases,
// post frames to it, relay what it reports (decoder state, decoded frames, capture results) into the
// session state and the receiver's callbacks, and tear it down. It owns the capture waiters; the
// worker handle, the decoder epoch, recovery state and the other shared fields are on the session
// state. The decoder itself runs in wss-worker.js (wss-pipeline.js); what a presented frame means
// for the page (layout, first-frame handshake, frame counters) goes to the receiver through callbacks.
import { cropGain } from "./sharp-viewport.js";
import { createWssWorkerClient } from "./wss-worker-client.js";

const MSG_LOG = "log";
const MSG_TRACE = "trace";
const MSG_CAPTURE_RESULT = "capture-result";
const MSG_WORKER_ERROR = "worker-error";
const MSG_DECODER_CONFIGURED = "decoder-configured";
const MSG_FAILURE = "failure";
const MSG_DROP = "drop";
const MSG_KEY_ACCEPTED = "key-accepted";
const MSG_KEY_FED = "key-fed";
const MSG_CODEC_FOLLOW = "codec-follow";
const MSG_OUT = "out";
const POST_FRAME = "frame";
const POST_RESYNC = "resync";
const POST_CAPTURE = "capture";
const RESYNC_REASON_START = "worker-start";
const RECOVERY_AWAITING_KEYFRAME = "awaiting-keyframe";
const RECOVERY_AWAITING_OUTPUT = "awaiting-keyframe-output";
const RECOVERY_HEALTHY = "healthy";
const CAPTURE_TIMEOUT_MS = 5_000;
const PRESENTED_TRACES_MAX = 64;
const GAIN_DIGITS = 1;
const LOG_START_FAILED = "wss-worker-unavailable";
const LOG_FAILURE = "wss-worker-failure";
const TRACE_READY = "wss-worker-ready";
const LOG_ERROR = "error";
const LOG_INFO = "info";
const LOG_WARN = "warn";

function fail(ctx, reason) {
  const env = ctx.env, state = env.state, hudState = env.hudState;
  env.log(LOG_ERROR, LOG_FAILURE, { reason });
  state.wssWorkerDisabled = true;
  hudState.dec = `worker:${reason}`;
  env.stopMedia();
  env.defer(env.requestMediaStart);
}

function teardown(ctx) {
  const env = ctx.env, state = env.state;
  const client = state.wssWorker;
  if (!client) return;
  state.wssWorker = null;
  state.wssCanvasDims = null;
  state.wssWorkerHybrid = null;
  const transferred = client.transferred;
  client.stop();
  for (const waiter of ctx.waiters.values()) waiter(null);
  ctx.waiters.clear();
  if (transferred) env.restoreCanvases();
}

// Hand both canvases to the worker. Until it answers, the host start waits (state.wssWorkerInit),
// so no media arrives before the path is settled; if the worker cannot run, the canvases come back
// and decode stays on the main thread.
function begin(ctx) {
  const env = ctx.env, state = env.state, hudState = env.hudState;
  const client = (env.createClient || createWssWorkerClient)({
    onMessage: m => onMessage(ctx, m),
    onFatal: reason => fail(ctx, reason),
  });
  state.wssWorker = client;
  state.wssCanvasDims = { width: 0, height: 0 };
  state.wssWorkerHybrid = null;
  state.wssWorkerInit = client.start({
    canvases: env.canvases(),
    epoch: state.wssDecoderEpoch,
    codec: env.codecSession.codec,
  }).then(result => {
    if (state.wssWorker !== client) return false; // stopped while starting
    if (result.ok) {
      // A resync during the handshake moved the epoch the worker was started with.
      client.post({ type: POST_RESYNC, epoch: state.wssDecoderEpoch, reason: RESYNC_REASON_START });
      hudState.dec = `${env.codecSession.codec} wss (worker)`;
      env.trace(TRACE_READY, {});
      return true;
    }
    env.log(LOG_WARN, LOG_START_FAILED, { reason: result.reason });
    teardown(ctx);
    env.ensureMainCanvas();
    return false;
  });
}

// The frame bytes move to the worker, they are not copied. The epoch lets the worker drop frames
// sent before a resync.
function postFrame(ctx, frame) {
  const env = ctx.env, state = env.state, hudState = env.hudState;
  state.rtcStartupFallback?.stop();
  hudState.mediaGeneration = frame.generation;
  const bytes = frame.data;
  state.wssWorker.post({
    type: POST_FRAME,
    epoch: state.wssDecoderEpoch,
    codec: frame.codec,
    generation: frame.generation,
    sequence: frame.sequence,
    key: frame.key,
    timestampUs: frame.timestampUs,
    buffer: bytes.buffer,
    byteOffset: bytes.byteOffset,
    byteLength: bytes.byteLength,
    hevcCodec: env.hevcCodec(),
    hybridMode: env.hybridModeName(),
    desktopAvailable: state.desktopAvailable,
    view: env.cropForGeneration(frame.generation),
    frameTrace: frame.frameTrace,
  }, [bytes.buffer]);
}

// Ask the worker to render the PNG and read the timestamp pattern. `diagnostics()` is read when
// the result arrives.
function capture(ctx, { sharp, diagnostics }) {
  const env = ctx.env, state = env.state;
  const id = ++state.wssCaptureId;
  return new Promise(resolve => {
    const timer = env.setTimeout(() => { ctx.waiters.delete(id); resolve(null); }, CAPTURE_TIMEOUT_MS);
    ctx.waiters.set(id, m => {
      env.clearTimeout(timer);
      if (!m || m.error) { resolve(null); return; }
      resolve({
        pngDataUrl: m.pngDataUrl,
        diagnostics: {
          glassToGlass: m.glassToGlass,
          ...diagnostics(),
          frameTrace: { ...state.wssLatestFrameTrace },
          frameTraceHistory: [...state.wssPresentedFrameTraces.values()].map(trace => ({ ...trace })),
        },
      });
    });
    state.wssWorker.post({ type: POST_CAPTURE, id, sharp });
  });
}

function onMessage(ctx, m) {
  const env = ctx.env, state = env.state, hudState = env.hudState;
  if (!state.wssWorker) return;
  switch (m.type) {
    case MSG_LOG: env.log(m.level, m.name, m.fields); return;
    case MSG_TRACE: env.trace(m.name, m.fields); return;
    case MSG_CAPTURE_RESULT: {
      const waiter = ctx.waiters.get(m.id);
      if (waiter) { ctx.waiters.delete(m.id); waiter(m); }
      return;
    }
    case MSG_WORKER_ERROR: fail(ctx, m.message || MSG_WORKER_ERROR); return;
    default: break;
  }
  // Everything else describes decoder state: ignore what predates the last resync.
  if (m.epoch !== state.wssDecoderEpoch) return;
  switch (m.type) {
    case MSG_DECODER_CONFIGURED: hudState.dec = `${m.codec} (worker)`; break;
    case MSG_FAILURE:
      if (m.hud) hudState.dec = m.hud;
      env.noteFailure(m.reason);
      break;
    case MSG_DROP:
      hudState.dropped++;
      if (m.recover) env.beginRecovery(m.recover);
      break;
    case MSG_KEY_ACCEPTED: onKeyAccepted(ctx, m); break;
    case MSG_KEY_FED: env.armKeyframeWatchdog(); break;
    case MSG_CODEC_FOLLOW: onCodecFollow(ctx, m); break;
    case MSG_OUT: onFrameOut(ctx, m); break;
    default: break;
  }
}

function onKeyAccepted(ctx, m) {
  const env = ctx.env, state = env.state;
  if (state.wssRecoveryState !== RECOVERY_AWAITING_KEYFRAME) return;
  state.wssRecoveryPending = true;
  state.wssRecoveryState = RECOVERY_AWAITING_OUTPUT;
  env.log(LOG_INFO, "wss-recovery-keyframe", { generation: m.generation, sequence: m.sequence, codec: m.codec });
}

function onCodecFollow(ctx, m) {
  const env = ctx.env, state = env.state;
  if (!env.codecSession.followBitstreamCodec(m.codec)) return;
  env.clearKeyframeWatchdog();
  env.resetRenderPacing();
  state.wssPresentedFrameTraces.clear();
  state.wssNoOutputFailures = 0;
  if (state.wssRecoveryState === RECOVERY_AWAITING_OUTPUT) state.wssRecoveryState = RECOVERY_AWAITING_KEYFRAME;
  env.log(LOG_INFO, "decoder-keyframe-codec-change", { previousCodec: m.previousCodec, codec: m.codec });
}

// The worker decoded (and maybe drew) a frame: the receiver-side bookkeeping that
// onDecodedFrame/drawFrame do on the main-thread path.
function onFrameOut(ctx, m) {
  const env = ctx.env, state = env.state;
  env.clearKeyframeWatchdog();
  state.wssNoOutputFailures = 0;
  if (m.decodedSequence !== null) env.wssV4.decodedSeq = m.decodedSequence;
  if (m.key && state.wssRecoveryState === RECOVERY_AWAITING_OUTPUT) {
    state.wssRecoveryPending = false;
    state.wssRecoveryState = RECOVERY_HEALTHY;
  }
  env.noteFrameReceived(m);
  if (!m.attempted) { env.wssV4.presentSkips++; return; }
  env.frameTelemetry.noteImmediateAttempt();
  if (m.hybrid) state.wssWorkerHybrid = m.hybrid;
  if (m.width) env.noteDecodedDimensions({ width: m.width, height: m.height });
  if (!m.presented) return;
  layoutPresented(ctx, m);
  env.firstFramePresented(m);
  env.frameTelemetry.noteImmediatePresented();
  state.wssLatestFrameTrace = { ...m.trace };
  state.wssPresentedFrameTraces.set(m.trace.sequence >>> 0, { ...m.trace });
  while (state.wssPresentedFrameTraces.size > PRESENTED_TRACES_MAX) {
    state.wssPresentedFrameTraces.delete(state.wssPresentedFrameTraces.keys().next().value);
  }
  env.notePresented(m);
  state.wssPresentedSequence = m.sequence;
}

function layoutPresented(ctx, m) {
  const env = ctx.env, state = env.state, hudState = env.hudState;
  const w = m.width, h = m.height;
  if (m.sharp) {
    env.layoutSharpCanvas(m.view);
    const gain = cropGain(m.view, w, h);
    hudState.dimensions = `${m.view.width}x${m.view.height}→${w}x${h} ${gain.x.toFixed(GAIN_DIGITS)}x/${gain.y.toFixed(GAIN_DIGITS)}x`;
    return;
  }
  if (m.resized) {
    state.wssCanvasDims = { width: w, height: h };
    env.syncViewportAndFollow();
  }
  env.layoutSharpCanvas(null);
  hudState.dimensions = `${w}x${h}`;
}

export function createWssDecode(env) {
  const ctx = { env, waiters: new Map() };
  return {
    begin: () => begin(ctx),
    teardown: () => teardown(ctx),
    fail: reason => fail(ctx, reason),
    postFrame: frame => postFrame(ctx, frame),
    capture: options => capture(ctx, options),
    onMessage: m => onMessage(ctx, m),
  };
}
