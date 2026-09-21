import { fork } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { artifactStore } from "./stream-lab-core.mjs";
import { LiveStreamLabSession, collectIdleEvidence, nativeFrameState as nativeDecodeState } from "./stream-lab-live.mjs";
import { startDirectLabRelay } from "./stream-lab-direct-relay.mjs";

const root = resolve(import.meta.dirname, "../..");
const delay = ms => new Promise(resolveWait => setTimeout(resolveWait, ms));

export function nativeFrameState(snapshot, monitor) {
  const selected = snapshot?.signaling?.monitors?.filter(item => item.selected);
  const native = snapshot?.nativePresentation;
  const ready = snapshot?.signaling?.registered === true && selected?.length === 1 &&
    selected[0].selector.toLowerCase() === monitor.toLowerCase() &&
    /^(host|srflx|prflx|relay)->(host|srflx|prflx|relay)$/.test(snapshot.route ?? "") &&
    nativeDecodeState(snapshot).ready &&
    Number.isFinite(snapshot.renderedAgeMs) && snapshot.renderedAgeMs >= 0 && snapshot.renderedAgeMs < 1000;
  return { ready, frames: native?.framesPresented ?? 0, firstFrameAtMs: native?.firstFrameAtMs,
    registeredAtMs: snapshot?.signaling?.registeredAtMs, route: snapshot?.route,
    codec: snapshot?.rtc?.codec?.mimeType, frameAgeMs: snapshot?.renderedAgeMs,
    fps: native?.fps, maxFrameGapMs: native?.maxFrameGapMs };
}

async function waitNative(session, monitor, timeoutMs = 20_000, options = {}) {
  const deadline = Date.now() + timeoutMs;
  let previous = null, snapshot;
  while (Date.now() < deadline) {
    session.assertLeaseHealthy();
    snapshot = await session.snapshot();
    const state = nativeFrameState(snapshot, monitor);
    if (state.firstFrameAtMs === options.previousFirstFrameAtMs) state.ready = false;
    if (state.ready && options.allowStatic) return state;
    if (state.ready && previous?.ready && state.firstFrameAtMs === previous.firstFrameAtMs &&
        state.registeredAtMs === previous.registeredAtMs && state.frames > previous.frames) return state;
    previous = state;
    await delay(250);
  }
  const error = new Error(`Native HEVC did not present advancing frames on ${monitor}`);
  error.diagnostics = snapshot;
  throw error;
}

async function selectMonitor(session, monitor, allowStatic = false) {
  const previousFirstFrameAtMs = (await session.snapshot())?.nativePresentation?.firstFrameAtMs;
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    session.assertLeaseHealthy();
    const accepted = await session.receiver.evaluate(
      `window.__bubbleplayDiagnostics?.selectMonitor?.(${JSON.stringify(monitor)}) ?? false`);
    if (accepted) return waitNative(session, monitor, 20_000, { allowStatic, previousFirstFrameAtMs });
    await delay(250);
  }
  throw new Error(`Receiver rejected monitor selection: ${monitor}`);
}

async function waitWss(session, monitor) {
  const deadline = Date.now() + 15_000;
  let previous = null, snapshot;
  while (Date.now() < deadline) {
    session.assertLeaseHealthy();
    snapshot = await session.snapshot();
    const selected = snapshot?.signaling?.monitors?.filter(item => item.selected);
    const fresh = snapshot?.route === "wss" && snapshot.signaling?.registered === true &&
      selected?.length === 1 && selected[0].selector.toLowerCase() === monitor.toLowerCase() &&
      snapshot.framesDrawn > 0 && Number.isFinite(snapshot.renderedAgeMs) &&
      snapshot.renderedAgeMs >= 0 && snapshot.renderedAgeMs < 1000;
    if (fresh && previous && snapshot.framesDrawn > previous.framesDrawn &&
        snapshot.signaling.registeredAtMs === previous.signaling.registeredAtMs)
      return { framesDrawn: snapshot.framesDrawn, renderedAgeMs: snapshot.renderedAgeMs,
        registeredAtMs: snapshot.signaling.registeredAtMs };
    previous = fresh ? snapshot : null;
    await delay(250);
  }
  const error = new Error("WSS did not recover between native cycles");
  error.diagnostics = snapshot;
  throw error;
}

async function sampleNative(session, monitor, seconds, store) {
  const initial = await waitNative(session, monitor);
  const started = performance.now();
  let previous = initial, nextProgress = 60;
  const samples = [];
  while (performance.now() - started < seconds * 1000) {
    await delay(1000);
    session.assertLeaseHealthy();
    const snapshot = await session.snapshot();
    const state = nativeFrameState(snapshot, monitor);
    const elapsedSeconds = (performance.now() - started) / 1000;
    store.appendTelemetry({ phase: "native-soak", elapsedSeconds, ...state });
    if (!state.ready || state.firstFrameAtMs !== initial.firstFrameAtMs ||
        state.registeredAtMs !== initial.registeredAtMs || state.frames <= previous.frames) {
      const error = new Error(`Native HEVC continuity failed at ${elapsedSeconds.toFixed(1)}s: ${JSON.stringify(state)}`);
      error.diagnostics = snapshot;
      throw error;
    }
    samples.push(state);
    previous = state;
    if (elapsedSeconds >= nextProgress) { console.log(`Native soak: ${Math.floor(elapsedSeconds)}s, ${state.fps} fps`); nextProgress += 60; }
  }
  return { result: "complete", seconds: (performance.now() - started) / 1000, samples: samples.length,
    presentedFps: (previous.frames - initial.frames) / ((performance.now() - started) / 1000),
    maxFrameGapMs: Math.max(...samples.map(sample => sample.maxFrameGapMs ?? 0)),
    latency: "not measured by native playback callbacks" };
}

async function main() {
  const { values } = parseArgs({ options: {
    "candidate-host": { type: "string", default: resolve(root, "host/target/release/bubbleplay-host.exe") },
    cycles: { type: "string", default: "10" }, "soak-seconds": { type: "string", default: "600" },
    "idle-seconds": { type: "string", default: "60" }, "relay-child": { type: "boolean", default: false },
  } });
  if (values["relay-child"]) {
    const relay = await startDirectLabRelay({ port: 0, publicDir: resolve(root, "worker/public") });
    process.send({ url: relay.url });
    process.on("message", async message => { if (message === "stop") { await relay.close(); process.exit(0); } });
    process.on("disconnect", async () => { await relay.close(); process.exit(0); });
    return;
  }
  const cycles = Number(values.cycles), soakSeconds = Number(values["soak-seconds"]), idleSeconds = Number(values["idle-seconds"]);
  if (![cycles, soakSeconds, idleSeconds].every(value => Number.isInteger(value) && value > 0))
    throw new Error("Cycle and duration arguments must be positive integers");
  const runId = randomUUID(), store = artifactStore(resolve(root, "artifacts/stream-lab"), runId);
  const report = { runId, startedAt: new Date().toISOString(), candidateHost: resolve(values["candidate-host"]), cycles: [] };
  report.candidateHostSha256 = createHash("sha256").update(readFileSync(report.candidateHost)).digest("hex");
  mkdirSync(resolve(root, "artifacts"), { recursive: true });
  const reportPath = resolve(root, `artifacts/native-hevc-${runId}.json`);
  const save = () => writeFileSync(reportPath, JSON.stringify(report, null, 2));
  store.initialize({ purpose: "native-hevc-acceptance", ...report });
  const relay = fork(fileURLToPath(import.meta.url), ["--relay-child"], { windowsHide: true, stdio: ["ignore", "ignore", "inherit", "ipc"] });
  const relayExit = new Promise(resolveExit => relay.once("exit", resolveExit));
  let session;
  try {
    const baseUrl = await new Promise((resolveUrl, reject) => {
      relay.once("message", message => resolveUrl(message.url));
      relay.once("error", reject);
      relay.once("exit", code => reject(new Error(`Relay exited during startup: ${code}`)));
    });
    session = new LiveStreamLabSession({ runId, store, baseUrl, candidateHost: report.candidateHost, receiverAssetOverride: false });
    // Native video frame callbacks require a visible receiver. Keep it off the
    // captured VDD; the synthetic source owns that display.
    await session.start("wss", { mode: "manual" });
    await session.startSource();
    const receiverDisplay = session.preRunDisplays.find(display =>
      display.deviceName.toLowerCase() !== session.display.deviceName.toLowerCase());
    if (!receiverDisplay) throw new Error("Native acceptance requires an uncaptured receiver display");
    const receiverWindow = await session.receiver.call("Browser.getWindowForTarget");
    await session.receiver.call("Browser.setWindowBounds", { windowId: receiverWindow.windowId,
      bounds: { windowState: "normal" } });
    await session.receiver.call("Browser.setWindowBounds", { windowId: receiverWindow.windowId,
      bounds: { left: receiverDisplay.x, top: receiverDisplay.y,
        width: Math.min(1000, receiverDisplay.width), height: Math.min(760, receiverDisplay.height) } });
    await session.receiver.call("Page.bringToFront");
    await session.selectVirtualMonitor();
    await session.source.evaluate("window.__bubbleplaySource.setScene('motion-60',469); window.__bubbleplaySource.resume(); true");
    await session.source.call("Page.bringToFront");
    report.windows = { receiver: await session.receiver.call("Browser.getWindowForTarget"),
      source: await session.source.call("Browser.getWindowForTarget") };
    report.sourceStarted = await session.source.evaluate("({...window.__bubbleplaySource.metadata(), visibility:document.visibilityState, screenX, screenY})");
    await delay(1500);
    report.initialWss = await session.snapshot();
    const target = session.display.deviceName;
    for (let index = 0; index < cycles; index++) {
      await session.switchTransport("webrtc", { relay: false, adaptive: false, decoderPath: "native" });
      await session.receiver.evaluate("window.__bubbleplayDiagnostics?.setStreamMode?.('game') ?? false");
      await session.selectVirtualMonitor();
      const ready = await waitNative(session, target);
      const other = (await session.snapshot()).signaling.monitors.find(item => item.selector.toLowerCase() !== target.toLowerCase());
      if (!other) throw new Error("Monitor-switch acceptance requires a second display");
      // An uncaptured desktop may be static. Require a new native peer's
      // presented frame there, then advancing motion on return to the source.
      await selectMonitor(session, other.selector, true);
      const returned = await selectMonitor(session, target);
      await session.switchTransport("wss", { adaptive: false });
      await session.selectVirtualMonitor();
      const wss = await waitWss(session, target);
      report.cycles.push({ cycle: index + 1, result: "complete", native: ready, returned, wss }); save();
      console.log(`Native monitor/transport cycle ${index + 1}/${cycles} complete`);
    }
    await session.switchTransport("webrtc", { relay: false, adaptive: false, decoderPath: "native" });
    await session.selectVirtualMonitor();
    report.soak = await sampleNative(session, target, soakSeconds, store);
  } catch (error) {
    report.error = { message: error.message, code: error.code, detail: error.detail, diagnostics: error.diagnostics };
    save();
    try { report.sourceAtFailure = await session?.source?.evaluate("({...window.__bubbleplaySource.metadata(), visibility:document.visibilityState, screenX, screenY})"); } catch {}
    if (session?.receiver && session?.source) {
      for (const [name, page] of [["receiver", session.receiver], ["source", session.source]]) {
        try { const shot = await page.call("Page.captureScreenshot", { format: "png" });
          writeFileSync(resolve(root, `artifacts/native-${runId}-${name}.png`), Buffer.from(shot.data, "base64"));
        } catch {}
      }
    }
  } finally {
    if (session) {
      try { await session.stop(); report.cleanup = session.cleanupEvidence;
        report.idle = await collectIdleEvidence(idleSeconds, { cleanupEvidence: report.cleanup, preCampaignSystem: session.preCampaignSystem });
      } catch (error) { report.cleanupError = error.message; }
    }
    if (relay.connected) relay.send("stop");
    await relayExit;
    report.result = report.error || report.cleanupError ? "fail" : report.idle?.result === "complete" ? "complete" : "unsupported";
    report.completedAt = new Date().toISOString(); save();
    console.log(`${report.result}: ${reportPath}`);
    if (report.result !== "complete") process.exitCode = 1;
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  main().catch(error => { console.error(error); process.exitCode = 1; });
