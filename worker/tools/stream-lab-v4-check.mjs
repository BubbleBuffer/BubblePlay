// Local WSS v4 live check on the loopback direct Stream Lab relay.
//
// Uses only local-only lab routes: the direct relay (no Worker, no approval
// gate, no application login), an uninstalled candidate host capturing the
// local desktop, and a headless Chromium receiver driven over CDP. The relay
// does not authenticate, so the candidate host gets fixed placeholder values
// instead of any real credential. No Windows service is touched.
//
//   node worker/tools/stream-lab-v4-check.mjs [--candidate-host <exe>]
//     [--static-seconds 60] [--motion-seconds 20] [--soak-seconds 600]
//
// Media-socket traffic is counted from CDP WebSocket frame events, so the
// numbers are what the browser actually received/sent on the v4 socket.
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { closeSync, existsSync, mkdirSync, openSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { parseArgs } from "node:util";
import { startDirectLabRelay } from "./stream-lab-direct-relay.mjs";
import { buildLabHostUrl, buildReceiverChromeArgs, buildReceiverUrl, CdpPage } from "./stream-lab-live.mjs";
import { findChrome } from "./stream-lab-runtime.mjs";
import { V4_FLAG_KEY, V4_FLAG_REFINE, V4_HEADER_BYTES } from "../public/media-v4.js";

const root = resolve(import.meta.dirname, "../..");
const delay = ms => new Promise(done => setTimeout(done, ms));
const PLACEHOLDER_CREDENTIAL = "local-lab-relay-placeholder";

// A small animated page for the motion and soak phases (placed on the
// captured primary display). 60 fps canvas scroll plus a moving block.
const STIMULUS = `data:text/html,${encodeURIComponent(`<!doctype html><title>v4 stimulus</title>
<body style="margin:0;background:#111"><canvas id=c width=640 height=360></canvas><script>
const c=document.getElementById('c'),g=c.getContext('2d');let t=0;
(function f(){t++;g.fillStyle='hsl('+(t%360)+',60%,40%)';g.fillRect(0,0,640,360);
g.fillStyle='#fff';g.fillRect((t*7)%640,120,80,80);g.font='32px monospace';g.fillText('frame '+t,20,60);
requestAnimationFrame(f)})();</script>`)}`;

export function parseV4Header(base64) {
  const bytes = Buffer.from(base64, "base64");
  if (bytes.byteLength < V4_HEADER_BYTES || bytes.readUInt32BE(0) !== 0x42505634) return null;
  const flags = bytes[5];
  return { key: (flags & V4_FLAG_KEY) !== 0, refine: (flags & V4_FLAG_REFINE) !== 0,
    seq: bytes.readUInt32BE(12), width: bytes.readUInt16BE(24), height: bytes.readUInt16BE(26),
    bytes: bytes.byteLength };
}

class MediaCounter {
  constructor() { this.sockets = new Map(); this.reset(); }
  reset() {
    this.window = { rxBinary: 0, rxBinaryBytes: 0, rxText: 0, rxTextBytes: 0, txText: 0, txTextBytes: 0,
      keys: 0, refines: 0, rxTextTypes: {}, txTextTypes: {}, firstFrameAt: null, sizes: new Set() };
    this.startedAt = performance.now();
  }
  attach(page) {
    page.on("Network.webSocketCreated", ({ requestId, url }) => {
      this.sockets.set(requestId, url.includes("/lab-media-ws") ? "media" : url.includes("/lab-ws") ? "signal" : "other");
    });
    page.on("Network.webSocketFrameReceived", ({ requestId, response }) => this.frame(requestId, response, "rx"));
    page.on("Network.webSocketFrameSent", ({ requestId, response }) => this.frame(requestId, response, "tx"));
  }
  frame(requestId, response, direction) {
    if (this.sockets.get(requestId) !== "media") return;
    const w = this.window;
    if (response.opcode === 2) {
      const header = parseV4Header(response.payloadData);
      w.rxBinary++;
      w.rxBinaryBytes += Buffer.byteLength(response.payloadData, "base64");
      if (header?.key) w.keys++;
      if (header?.refine) w.refines++;
      if (header) w.sizes.add(`${header.width}x${header.height}`);
      w.firstFrameAt ??= performance.now();
      return;
    }
    let type = "?";
    try { type = JSON.parse(response.payloadData).type; } catch {}
    const bytes = Buffer.byteLength(response.payloadData);
    if (direction === "rx") { w.rxText++; w.rxTextBytes += bytes; w.rxTextTypes[type] = (w.rxTextTypes[type] ?? 0) + 1; }
    else { w.txText++; w.txTextBytes += bytes; w.txTextTypes[type] = (w.txTextTypes[type] ?? 0) + 1; }
  }
  summary() {
    const seconds = (performance.now() - this.startedAt) / 1000;
    const w = this.window;
    return { seconds: +seconds.toFixed(1), ...w, sizes: [...w.sizes], firstFrameAt: undefined,
      rxBinaryKbps: +(w.rxBinaryBytes * 8 / 1000 / Math.max(seconds, 0.001)).toFixed(1),
      rxFramesPerSecond: +(w.rxBinary / Math.max(seconds, 0.001)).toFixed(1) };
  }
}

async function snapshot(page) {
  return page.evaluate(`(() => { const s = window.__bubbleplayDiagnostics?.snapshot?.(); if (!s) return null;
    return { framesDrawn: s.framesDrawn, renderedAgeMs: s.renderedAgeMs, fps: s.fps, resyncs: s.resyncs,
      dropped: s.dropped, keyframes: s.keyframes, maxFrameGap: s.maxFrameGap, dec: s.dec,
      protocol: s.wssMedia?.protocol, v4: s.wssMedia?.v4, route: s.route,
      status: document.querySelector('#connection-status, [data-connection-status]')?.textContent?.trim() ?? null }; })()`);
}

async function waitFor(page, predicate, timeoutMs, pollMs = 50) {
  const deadline = performance.now() + timeoutMs;
  let last = null;
  while (performance.now() < deadline) {
    last = await snapshot(page).catch(() => null);
    if (last && predicate(last)) return last;
    await delay(pollMs);
  }
  const error = new Error("timed out waiting for receiver state");
  error.last = last;
  throw error;
}

async function main() {
  const { values } = parseArgs({ options: {
    "candidate-host": { type: "string", default: resolve(root, "host/target/release/bubbleplay-host.exe") },
    "static-seconds": { type: "string", default: "60" },
    "motion-seconds": { type: "string", default: "20" },
    "soak-seconds": { type: "string", default: "600" },
    "cdp-port": { type: "string", default: "9337" },
    // Capture this monitor during the static phase (selector suffix, e.g. "DISPLAY2")
    // when the primary display has live content; motion runs on the primary.
    "static-monitor": { type: "string" },
  } });
  const seconds = name => {
    const value = Number(values[name]);
    if (!Number.isInteger(value) || value < 0) throw new RangeError(`--${name} must be a non-negative integer`);
    return value;
  };
  const staticSeconds = seconds("static-seconds"), motionSeconds = seconds("motion-seconds"), soakSeconds = seconds("soak-seconds");
  const chrome = findChrome();
  if (!chrome) throw new Error("Chrome or Edge was not found");
  if (!existsSync(values["candidate-host"])) throw new Error(`candidate host missing: ${values["candidate-host"]}`);
  const runId = randomUUID();
  const dir = resolve(root, "artifacts/stream-lab-v4", runId);
  mkdirSync(dir, { recursive: true });
  const report = { runId, startedAt: new Date().toISOString(), candidateHost: values["candidate-host"], phases: {} };
  const save = () => writeFileSync(resolve(dir, "report.json"), JSON.stringify(report, null, 2));
  const relay = await startDirectLabRelay({ port: 0, publicDir: resolve(root, "worker/public") });
  let host, receiverProcess, stimulusProcess, receiver;
  try {
    const out = openSync(resolve(dir, "host.stdout.log"), "a"), err = openSync(resolve(dir, "host.stderr.log"), "a");
    const hostStartedAt = performance.now();
    host = spawn(values["candidate-host"], ["--mode", "serve", "--worker", buildLabHostUrl(relay.url, runId).href], {
      cwd: resolve(root, "host"), stdio: ["ignore", out, err], windowsHide: true,
      env: { ...process.env, RUST_LOG: "info",
        BUBBLEPLAY_TOKEN: PLACEHOLDER_CREDENTIAL, BUBBLEPLAY_HOST_GATE_TOKEN: PLACEHOLDER_CREDENTIAL },
    });
    closeSync(out); closeSync(err);
    host.once("exit", code => { report.hostExit = code; });

    const url = buildReceiverUrl(relay.url, "wss", { labRun: runId, adaptive: true });
    const port = Number(values["cdp-port"]);
    const args = buildReceiverChromeArgs(resolve(dir, "receiver-profile"), port, "about:blank", { startMinimized: false });
    receiverProcess = spawn(chrome, ["--headless=new", ...args], { stdio: "ignore", windowsHide: true });
    receiver = await new CdpPage(port).open();
    const counter = new MediaCounter();
    counter.attach(receiver);
    await receiver.call("Network.enable");
    await receiver.call("Emulation.setDeviceMetricsOverride", { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false });
    const navigatedAt = performance.now();
    await receiver.navigate(url.href);

    // 1. First frame.
    const first = await waitFor(receiver, s => s.framesDrawn > 0 && s.protocol === 4, 30_000);
    report.phases.firstFrame = {
      msFromReceiverNavigate: Math.round(performance.now() - navigatedAt),
      msFromHostSpawn: Math.round(performance.now() - hostStartedAt),
      firstBinaryMsFromNavigate: counter.window.firstFrameAt ? Math.round(counter.window.firstFrameAt - navigatedAt) : null,
      snapshot: first,
    };
    save();
    console.log("first frame", report.phases.firstFrame.msFromReceiverNavigate, "ms");

    // 2. Static desktop: settle (refine lands ~300 ms after the last change).
    const primaryMonitor = (await receiver.evaluate(
      "window.__bubbleplayDiagnostics?.snapshot?.()?.signaling?.monitors?.find(m => m.selected)?.selector ?? null"));
    if (values["static-monitor"]) {
      const before = await snapshot(receiver);
      const monitors = await receiver.evaluate("window.__bubbleplayDiagnostics?.snapshot?.()?.signaling?.monitors ?? []");
      const wanted = values["static-monitor"].toLowerCase();
      const selector = monitors.find(m => m.selector.toLowerCase().endsWith(wanted))?.selector;
      if (!selector) throw new Error(`no monitor matches ${values["static-monitor"]}: ${JSON.stringify(monitors)}`);
      const accepted = await receiver.evaluate(`window.__bubbleplayDiagnostics.selectMonitor(${JSON.stringify(selector)})`);
      if (!accepted) throw new Error(`receiver rejected static monitor ${selector}`);
      await waitFor(receiver, s => s.framesDrawn > before.framesDrawn, 15_000);
      report.staticMonitor = selector;
    }
    await delay(3_000);
    counter.reset();
    const staticStart = await snapshot(receiver);
    await delay(staticSeconds * 1000);
    report.phases.static = { ...counter.summary(), start: staticStart, end: await snapshot(receiver) };
    report.monitors = await receiver.evaluate("window.__bubbleplayDiagnostics?.snapshot?.()?.signaling?.monitors ?? null").catch(() => null);
    try {
      const shot = await receiver.call("Page.captureScreenshot", { format: "png" });
      writeFileSync(resolve(dir, "receiver-static.png"), Buffer.from(shot.data, "base64"));
    } catch {}
    save();
    console.log("static", JSON.stringify(report.phases.static));

    // 3. Motion (on the primary display, where the stimulus window opens).
    if (values["static-monitor"] && primaryMonitor) {
      const before = await snapshot(receiver);
      await receiver.evaluate(`window.__bubbleplayDiagnostics.selectMonitor(${JSON.stringify(primaryMonitor)})`);
      await waitFor(receiver, s => s.framesDrawn > before.framesDrawn, 15_000);
    }
    const stimulusProfile = resolve(dir, "stimulus-profile");
    counter.reset();
    const motionStart = await snapshot(receiver);
    stimulusProcess = spawn(chrome, [`--user-data-dir=${stimulusProfile}`, "--no-first-run",
      "--window-position=40,40", "--window-size=660,420", `--app=${STIMULUS}`], { stdio: "ignore" });
    await delay(motionSeconds * 1000);
    const motionEnd = await snapshot(receiver);
    report.phases.motion = { ...counter.summary(), start: motionStart, end: motionEnd,
      presentedFps: +((motionEnd.framesDrawn - motionStart.framesDrawn) / motionSeconds).toFixed(1) };
    save();
    console.log("motion", JSON.stringify(report.phases.motion));

    // 4. Media socket drop -> recovery without reload (motion still running).
    const beforeDrop = await snapshot(receiver);
    const droppedAt = performance.now();
    const dropped = await receiver.evaluate("window.__bubbleplayDiagnostics?.dropWssV4Socket?.() ?? false");
    const reopened = await waitFor(receiver, s => s.v4?.reconnects > (beforeDrop.v4?.reconnects ?? 0) && s.v4?.socketOpen, 15_000);
    const reopenMs = Math.round(performance.now() - droppedAt);
    const recovered = await waitFor(receiver, s => s.v4?.frames > reopened.v4.frames && s.framesDrawn > reopened.framesDrawn && !s.v4.waitingForKey, 15_000);
    report.phases.drop = { dropped, socketReopenMs: reopenMs, firstFrameAfterDropMs: Math.round(performance.now() - droppedAt),
      pageReloaded: false, before: beforeDrop, after: recovered };
    save();
    console.log("drop", JSON.stringify(report.phases.drop));

    // 5. Soak with motion.
    const soakStart = await snapshot(receiver);
    counter.reset();
    const samples = [];
    let maxRenderedAgeMs = 0;
    for (let elapsed = 0; elapsed < soakSeconds; elapsed += 5) {
      await delay(5_000);
      const sample = await snapshot(receiver);
      maxRenderedAgeMs = Math.max(maxRenderedAgeMs, sample.renderedAgeMs ?? 0);
      samples.push({ t: elapsed + 5, framesDrawn: sample.framesDrawn, resyncs: sample.resyncs,
        maxFrameGap: sample.maxFrameGap, reconnects: sample.v4?.reconnects, gateDrops: sample.v4?.gateDrops,
        presentSkips: sample.v4?.presentSkips, status: sample.v4?.status, renderedAgeMs: Math.round(sample.renderedAgeMs ?? -1) });
      if (report.hostExit !== undefined) throw new Error(`candidate host exited during soak: ${report.hostExit}`);
    }
    const soakEnd = await snapshot(receiver);
    report.phases.soak = { ...counter.summary(), start: soakStart, end: soakEnd,
      decoderResets: soakEnd.resyncs - soakStart.resyncs,
      reconnects: (soakEnd.v4?.reconnects ?? 0) - (soakStart.v4?.reconnects ?? 0),
      gateDrops: (soakEnd.v4?.gateDrops ?? 0) - (soakStart.v4?.gateDrops ?? 0),
      maxFrameGapMs: soakEnd.maxFrameGap, maxSampledRenderedAgeMs: Math.round(maxRenderedAgeMs),
      presentedFps: soakSeconds ? +((soakEnd.framesDrawn - soakStart.framesDrawn) / soakSeconds).toFixed(1) : null,
      samples };
    save();
    console.log("soak", JSON.stringify({ ...report.phases.soak, samples: samples.length }));
  } catch (error) {
    report.error = { message: error.message, last: error.last ?? null };
    try { report.errorSnapshot = await snapshot(receiver); } catch {}
    process.exitCode = 1;
  } finally {
    try { await receiver?.call("Browser.close"); } catch {}
    for (const child of [stimulusProcess, receiverProcess, host]) {
      if (child?.pid) spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore", windowsHide: true });
    }
    await delay(1_000);
    await relay.close();
    report.finishedAt = new Date().toISOString();
    save();
    console.log(`report: ${resolve(dir, "report.json")}`);
  }
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(import.meta.filename)) await main();
