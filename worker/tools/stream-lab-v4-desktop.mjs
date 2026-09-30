// Local WSS v4 realistic-desktop check on the loopback direct Stream Lab relay.
//
// Same local-only routes as stream-lab-v4-check.mjs: the direct relay (no
// Worker, no approval gate, no login), an uninstalled candidate host with
// placeholder values instead of credentials, a headless receiver over CDP.
// No Windows service is touched.
//
//   node worker/tools/stream-lab-v4-desktop.mjs [--candidate-host <exe>]
//     [--label candidate] [--cycles 10] [--cycle-seconds 60] [--monitor DISPLAY2]
//     [--window 1480x960] [--host-args="--fixed-bitrate 1500000"]
//     [--throttle-kbps 2000] [--throttle-delay-ms 20]
//     [--scenario reading|scroll] [--pointer-over-stimulus]
//
// A browser window with a long article is opened on the captured monitor.
// Each cycle is half reading (a smooth scroll burst every 5 s) and half idle
// (a blinking caret and a small spinner). The receiver page is probed on
// every presented change: the share of 64x64 tiles that changed since the
// previous presented frame (a whole-frame dip or sharpen changes most tiles),
// and, during idle, per-tile luma PSNR against a host screenshot taken once
// the screen has settled. Media frames and flags are counted from CDP
// WebSocket frames on the v4 media socket.
//
// --throttle-kbps puts a FIFO bottleneck on the relay's host->client media
// path for the whole session (a slow link with a deep buffer). Each smooth
// scroll burst then reports the presented fps while it runs.
//
// --scenario scroll makes the reading half one continuous Chrome scroll of
// the article (back-to-back smooth bursts, no jump), so scroll dominates the
// screen; the report counts the host's per-second content class and the
// scroll detector / pointer-map totals. --pointer-over-stimulus parks the
// OS cursor over the article for the run (the host reads the real cursor)
// and puts it back afterwards.
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { startDirectLabRelay } from "./stream-lab-direct-relay.mjs";
import { buildLabHostUrl, buildReceiverChromeArgs, buildReceiverUrl, CdpPage } from "./stream-lab-live.mjs";
import { findChrome } from "./stream-lab-runtime.mjs";
import { parseV4Header } from "./stream-lab-v4-check.mjs";

const root = resolve(import.meta.dirname, "../..");
const delay = ms => new Promise(done => setTimeout(done, ms));
const PLACEHOLDER_CREDENTIAL = "local-lab-relay-placeholder";
/// Idle frames this long after the last scroll are "settled".
const SETTLE_MS = 2_000;
/// A presented frame changing at least this share of tiles while idle is a
/// whole-frame event (dip or sharpen).
const WHOLE_FRAME_SHARE = 0.3;

const WORDS = ("the of and to in is that for it as with was on be by at this from or which an are have not "
  + "stream frame tile quality refine budget encoder decoder latency window desktop screen pixel motion "
  + "network bandwidth packet socket relay receiver browser capture display render scroll text page").split(" ");

function article() {
  let seed = 7;
  const next = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
  const paragraphs = [];
  for (let p = 0; p < 160; p++) {
    const words = [];
    const length = 60 + Math.floor(next() * 80);
    for (let w = 0; w < length; w++) words.push(WORDS[Math.floor(next() * WORDS.length)]);
    words[0] = words[0][0].toUpperCase() + words[0].slice(1);
    paragraphs.push(`${p % 9 === 0 ? `<h2>Section ${p / 9 + 1}</h2>` : ""}<p>${words.join(" ")}.</p>`
      + (p % 5 === 2 ? `<canvas class=pic width=640 height=300 data-seed=${p}></canvas>` : ""));
  }
  return paragraphs.join("");
}

export const STIMULUS = `<!doctype html><title>v4 desktop stimulus</title>
<style>body{margin:0;font:16px/1.55 Segoe UI,Arial,sans-serif;color:#202124;background:#fff}
header{position:sticky;top:0;background:#f1f3f4;border-bottom:1px solid #dadce0;padding:10px 24px;font-weight:600}
main{display:grid;grid-template-columns:1fr 280px;gap:32px;padding:0 24px}article{max-width:820px}
aside{position:sticky;top:60px;align-self:start;padding-top:16px}textarea{width:100%;height:120px;font:14px Consolas,monospace}
.caret{display:inline-block;width:2px;height:18px;background:#202124;vertical-align:middle;animation:b 1.06s steps(1) infinite}
@keyframes b{50%{opacity:0}}.spin{width:22px;height:22px;border:3px solid #dadce0;border-top-color:#1a73e8;border-radius:50%;
visibility:hidden;animation:r .8s linear infinite}.spin.on{visibility:visible}@keyframes r{to{transform:rotate(360deg)}}</style>
<header>Stream quality notes</header><main><article>${article()}</article>
<aside><textarea autofocus>Draft</textarea><p>Typing here <span class=caret></span></p><div class=spin id=spin></div></aside></main>
<script>for(const c of document.querySelectorAll('canvas.pic')){const g=c.getContext('2d'),s=+c.dataset.seed;
const grad=g.createLinearGradient(0,0,640,300);grad.addColorStop(0,'hsl('+(s*37%360)+',55%,55%)');grad.addColorStop(1,'hsl('+(s*91%360)+',45%,30%)');
g.fillStyle=grad;g.fillRect(0,0,640,300);const d=g.getImageData(0,0,640,300),a=d.data;let r=s*9973;
for(let i=0;i<a.length;i+=4){r=(r*1103515245+12345)%2147483648;const n=(r>>16&63)-32;a[i]+=n;a[i+1]+=n;a[i+2]+=n}
g.putImageData(d,0,0);for(let k=0;k<12;k++){g.fillStyle='hsla('+(k*30+s)%360+',70%,60%,.6)';g.beginPath();
g.arc((k*97+s*13)%640,(k*53+s*7)%300,20+k*3,0,7);g.fill()}}
window.lab={scrollBurst(px,ms){const from=scrollY,t0=performance.now();return new Promise(done=>{
(function f(t){const k=Math.min(1,(t-t0)/ms);scrollTo(0,from+px*(1-Math.cos(Math.PI*k))/2);k<1?requestAnimationFrame(f):done(scrollY)})(t0)})},
jump(px){scrollBy(0,px);return scrollY},spinner(on){document.getElementById('spin').classList.toggle('on',on)},top(){scrollTo(0,0)}};</script>`;

// Injected into the receiver page: per presented change, the share of 64x64
// tiles whose luma changed and (with a reference) per-tile PSNR.
const PROBE = `(() => {
  if (window.__labQuality) return true;
  const T = 64, STEP = 2;
  const state = { records: [], ref: null, prev: null, running: true };
  const work = document.createElement("canvas");
  function source() {
    const sharp = document.getElementById("sharp-screen");
    return sharp?.classList.contains("active") ? sharp : document.getElementById("screen");
  }
  function luma(canvas) {
    const w = canvas.width, h = canvas.height;
    if (!w || !h) return null;
    const context = canvas.getContext("2d");
    if (!context) return null;
    const data = context.getImageData(0, 0, w, h).data;
    const sw = Math.ceil(w / STEP), sh = Math.ceil(h / STEP);
    const out = new Uint8Array(sw * sh);
    for (let y = 0, o = 0; y < h; y += STEP)
      for (let x = 0; x < w; x += STEP, o++) {
        const i = (y * w + x) * 4;
        out[o] = (data[i] * 77 + data[i + 1] * 150 + data[i + 2] * 29) >> 8;
      }
    return { w, h, sw, sh, y: out };
  }
  function tiles(a, b) {
    const cols = Math.ceil(a.w / T), rows = Math.ceil(a.h / T), per = T / STEP;
    const changed = [], psnr = [];
    for (let r = 0; r < rows; r++) for (let c = 0; c < cols; c++) {
      let sq = 0, abs = 0, n = 0;
      for (let y = r * per; y < Math.min((r + 1) * per, a.sh); y++)
        for (let x = c * per; x < Math.min((c + 1) * per, a.sw); x++) {
          const d = a.y[y * a.sw + x] - b.y[y * a.sw + x]; sq += d * d; abs += d < 0 ? -d : d; n++;
        }
      changed.push(abs / n > 1.0);
      const mse = sq / n;
      psnr.push(mse < 0.066 ? 60 : 10 * Math.log10(65025 / mse));
    }
    return { changed, psnr };
  }
  function tick() {
    if (!state.running) return;
    try {
      const canvas = source();
      const cur = canvas && luma(canvas);
      if (cur) {
        const record = { t: performance.now() };
        if (state.prev && state.prev.w === cur.w && state.prev.h === cur.h) {
          const { changed } = tiles(cur, state.prev);
          const count = changed.filter(Boolean).length;
          if (count === 0) { requestAnimationFrame(tick); return; }
          record.changedShare = count / changed.length;
        }
        if (state.ref && state.ref.w === cur.w && state.ref.h === cur.h) {
          const sorted = tiles(cur, state.ref).psnr.sort((x, y) => x - y);
          record.psnrMedian = sorted[Math.floor(sorted.length / 2)];
          record.psnrP10 = sorted[Math.floor(sorted.length / 10)];
        }
        state.prev = cur;
        state.records.push(record);
      }
    } catch (error) { state.error = String(error); }
    requestAnimationFrame(tick);
  }
  window.__labQuality = {
    take() { const r = state.records; state.records = []; return { records: r, error: state.error ?? null }; },
    async setReference(dataUrl) {
      if (!dataUrl) { state.ref = null; return true; }
      const image = new Image();
      await new Promise((ok, fail) => { image.onload = ok; image.onerror = fail; image.src = dataUrl; });
      work.width = image.width; work.height = image.height;
      work.getContext("2d").drawImage(image, 0, 0);
      state.ref = luma(work);
      return state.ref?.w ?? 0;
    },
  };
  requestAnimationFrame(tick);
  return true;
})()`;

function screenshot(deviceName, path) {
  const script = `Add-Type -AssemblyName System.Drawing,System.Windows.Forms
Add-Type 'using System;using System.Runtime.InteropServices;public class LabDpi{[DllImport("user32.dll")]public static extern bool SetProcessDPIAware();}'
[LabDpi]::SetProcessDPIAware() | Out-Null
$s = [System.Windows.Forms.Screen]::AllScreens | Where-Object { $_.DeviceName -eq '${deviceName.replace(/'/g, "''")}' } | Select-Object -First 1
if (-not $s) { $s = [System.Windows.Forms.Screen]::PrimaryScreen }
$b = New-Object System.Drawing.Bitmap $s.Bounds.Width, $s.Bounds.Height
$g = [System.Drawing.Graphics]::FromImage($b)
$g.CopyFromScreen($s.Bounds.X, $s.Bounds.Y, 0, 0, $b.Size)
$b.Save('${path.replace(/'/g, "''")}', [System.Drawing.Imaging.ImageFormat]::Png)
"$($s.Bounds.X),$($s.Bounds.Y)"`;
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], { encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(`screenshot failed: ${result.stderr}`);
  return result.stdout.trim();
}

function screenBounds(deviceName) {
  const script = `Add-Type -AssemblyName System.Windows.Forms
$s = [System.Windows.Forms.Screen]::AllScreens | Where-Object { $_.DeviceName -eq '${deviceName.replace(/'/g, "''")}' } | Select-Object -First 1
if (-not $s) { $s = [System.Windows.Forms.Screen]::PrimaryScreen }
"$($s.Bounds.X),$($s.Bounds.Y),$($s.Bounds.Width),$($s.Bounds.Height)"`;
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], { encoding: "utf8", windowsHide: true });
  const [x, y, width, height] = result.stdout.trim().split(",").map(Number);
  return { x, y, width, height };
}

/// Presented fps while each smooth scroll burst runs: the receiver probe
/// records one entry per presented change, so this is what the viewer saw.
/// Media fps counts v4 frames that arrived in the same window.
function summarizeBursts(bursts, frames, quality) {
  const per = bursts.map(b => {
    const seconds = (b.end - b.start) / 1000;
    const presented = quality.filter(q => q.t >= b.start && q.t < b.end).length;
    const media = frames.filter(f => f.t >= b.start && f.t < b.end);
    return { presentedFps: presented / seconds, mediaFps: media.length / seconds,
      kbps: media.reduce((s, f) => s + f.bytes, 0) * 8 / 1000 / seconds };
  });
  const pick = key => per.map(b => b[key]);
  return { bursts: per.length,
    presentedFps: { min: percentile(pick("presentedFps"), 0), p10: percentile(pick("presentedFps"), 0.1), median: percentile(pick("presentedFps"), 0.5) },
    mediaFps: { min: percentile(pick("mediaFps"), 0), median: percentile(pick("mediaFps"), 0.5) },
    kbpsMedian: percentile(pick("kbps"), 0.5) };
}

/// Seconds per dominant content class, from the host's per-second totals.
function contentSeconds(log) {
  const counts = {};
  for (const line of log.split("\n")) {
    if (!line.includes("WSS v4 link totals")) continue;
    const match = line.match(/content="?(\w+)/);
    if (match) counts[match[1]] = (counts[match[1]] ?? 0) + 1;
  }
  return counts;
}

/// Move the OS cursor; returns where it was.
function moveCursor(x, y) {
  const script = `Add-Type -Namespace Lab -Name Cursor -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);';
Add-Type -AssemblyName System.Windows.Forms; $p = [System.Windows.Forms.Cursor]::Position; "$($p.X),$($p.Y)";
[void][Lab.Cursor]::SetCursorPos(${Math.round(x)}, ${Math.round(y)})`;
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], { encoding: "utf8", windowsHide: true });
  const [px, py] = result.stdout.trim().split(",").map(Number);
  return Number.isFinite(px) && Number.isFinite(py) ? { x: px, y: py } : null;
}

function hostTotals(log) {
  const lines = log.split("\n").filter(line => line.includes("WSS v4 link totals"));
  const last = lines.at(-1);
  if (!last) return null;
  const fields = {};
  for (const match of last.matchAll(/(\w+)=("[^"]*"|\S+)/g)) fields[match[1]] = match[2].replace(/^"|"$/g, "");
  return fields;
}

const percentile = (values, p) => {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return +sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))].toFixed(2);
};

async function main() {
  const { values } = parseArgs({ options: {
    "candidate-host": { type: "string", default: resolve(root, "host/target/release/bubbleplay-host.exe") },
    label: { type: "string", default: "candidate" },
    cycles: { type: "string", default: "10" },
    "cycle-seconds": { type: "string", default: "60" },
    monitor: { type: "string", default: "DISPLAY2" },
    "cdp-port": { type: "string", default: "9341" },
    window: { type: "string", default: "1480x960" },
    // Extra host arguments, e.g. "--fixed-bitrate 1500000" for a constrained link.
    "host-args": { type: "string", default: "" },
    "throttle-kbps": { type: "string", default: "" },
    "throttle-delay-ms": { type: "string", default: "20" },
    scenario: { type: "string", default: "reading" },
    "pointer-over-stimulus": { type: "boolean", default: false },
  } });
  if (!["reading", "scroll"].includes(values.scenario)) throw new RangeError("--scenario reading|scroll");
  const cycles = Number(values.cycles), cycleSeconds = Number(values["cycle-seconds"]);
  if (!Number.isInteger(cycles) || cycles < 1 || !Number.isInteger(cycleSeconds) || cycleSeconds < 20)
    throw new RangeError("--cycles >= 1 and --cycle-seconds >= 20 are required");
  const chrome = findChrome();
  if (!chrome) throw new Error("Chrome or Edge was not found");
  if (!existsSync(values["candidate-host"])) throw new Error(`candidate host missing: ${values["candidate-host"]}`);
  const runId = randomUUID();
  const dir = resolve(root, "artifacts/stream-lab-v4-desktop", `${values.label}-${runId}`);
  mkdirSync(dir, { recursive: true });
  const report = { runId, label: values.label, startedAt: new Date().toISOString(), candidateHost: values["candidate-host"], cycles, cycleSeconds,
    scenario: values.scenario, pointerOverStimulus: values["pointer-over-stimulus"] };
  const save = () => writeFileSync(resolve(dir, "report.json"), JSON.stringify(report, null, 2));
  const throttle = values["throttle-kbps"]
    ? { kbps: Number(values["throttle-kbps"]), delayMs: Number(values["throttle-delay-ms"]) }
    : null;
  report.throttle = throttle;
  const relay = await startDirectLabRelay({ port: 0, publicDir: resolve(root, "worker/public"), throttle });
  let host, receiverProcess, stimulusProcess, receiver, stimulus, cursorWas = null;
  try {
    const out = openSync(resolve(dir, "host.stdout.log"), "a"), err = openSync(resolve(dir, "host.stderr.log"), "a");
    host = spawn(values["candidate-host"], ["--mode", "serve", "--worker", buildLabHostUrl(relay.url, runId).href,
      ...values["host-args"].split(" ").filter(Boolean)], {
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
    const frames = [];
    const sockets = new Map();
    receiver.on("Network.webSocketCreated", ({ requestId, url: socketUrl }) => sockets.set(requestId, socketUrl.includes("/lab-media-ws")));
    receiver.on("Network.webSocketFrameReceived", ({ requestId, response }) => {
      if (!sockets.get(requestId) || response.opcode !== 2) return;
      const header = parseV4Header(response.payloadData);
      frames.push({ t: performance.now(), bytes: Buffer.byteLength(response.payloadData, "base64"),
        key: header?.key ?? false, refine: header?.refine ?? false });
    });
    await receiver.call("Network.enable");
    await receiver.call("Emulation.setDeviceMetricsOverride", { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false });
    await receiver.navigate(url.href);
    const diag = expression => receiver.evaluate(`(() => { const s = window.__bubbleplayDiagnostics?.snapshot?.(); return s ? (${expression}) : null; })()`);
    const until = async (expression, timeoutMs) => {
      const deadline = performance.now() + timeoutMs;
      while (performance.now() < deadline) {
        if (await diag(expression).catch(() => false)) return;
        await delay(100);
      }
      throw new Error(`timed out waiting for ${expression}`);
    };
    await until("s.framesDrawn > 0 && s.wssMedia?.protocol === 4", 30_000);

    const monitors = await receiver.evaluate("window.__bubbleplayDiagnostics?.snapshot?.()?.signaling?.monitors ?? []");
    const wanted = values.monitor.toLowerCase();
    const monitor = monitors.find(m => m.selector.toLowerCase().endsWith(wanted)) ?? monitors.find(m => m.selected);
    if (!monitor) throw new Error(`no monitor: ${JSON.stringify(monitors)}`);
    if (!monitor.selected) {
      const before = await diag("s.framesDrawn");
      if (!await receiver.evaluate(`window.__bubbleplayDiagnostics.selectMonitor(${JSON.stringify(monitor.selector)})`))
        throw new Error(`receiver rejected monitor ${monitor.selector}`);
      await until(`s.framesDrawn > ${before}`, 15_000);
    }
    report.monitor = monitor;
    const bounds = screenBounds(monitor.selector);
    report.bounds = bounds;

    const stimulusPort = port + 1;
    const stimulusPath = resolve(dir, "stimulus.html");
    writeFileSync(stimulusPath, STIMULUS);
    stimulusProcess = spawn(chrome, [`--user-data-dir=${resolve(dir, "stimulus-profile")}`, "--no-first-run",
      `--remote-debugging-port=${stimulusPort}`, `--window-position=${bounds.x + 40},${bounds.y + 30}`,
      `--window-size=${values.window.replace("x", ",")}`, "--force-device-scale-factor=1", `--app=${pathToFileURL(stimulusPath).href}`], { stdio: "ignore" });
    stimulus = await new CdpPage(stimulusPort).open(20_000);
    await delay(1_500);
    await stimulus.evaluate("window.lab.top()");
    if (values["pointer-over-stimulus"]) {
      cursorWas = moveCursor(bounds.x + 40 + 400, bounds.y + 30 + 480);
      report.pointer = { x: bounds.x + 440, y: bounds.y + 510 };
    }
    await receiver.evaluate(PROBE);
    await delay(5_000);
    await receiver.evaluate("window.__labQuality.take()");

    const setupFrames = frames.length;
    const setupKeys = frames.filter(f => f.key).length;
    const windows = [];
    const bursts = [];
    const quality = [];
    const measureStart = performance.now();
    for (let cycle = 0; cycle < cycles; cycle++) {
      const half = cycleSeconds * 500;
      const readingStart = performance.now();
      await stimulus.evaluate("window.lab.spinner(false)");
      // Smooth scroll bursts, and one instant jump (a link / Page Down) that
      // replaces the article view without motion to predict from.
      const continuous = values.scenario === "scroll";
      for (let at = 1_000, burst = 0; at + 1_000 <= half; at += continuous ? 1_000 : 5_000, burst++) {
        await delay(Math.max(0, readingStart + at - performance.now()));
        const burstStart = performance.now();
        const jump = !continuous && burst === 2;
        await stimulus.evaluate(jump ? "window.lab.jump(2400)" : "window.lab.scrollBurst(420, 900)");
        if (!jump) bursts.push({ start: burstStart, end: performance.now() });
      }
      await delay(Math.max(0, readingStart + half - performance.now()));
      const idleStart = performance.now();
      windows.push({ kind: "reading", start: readingStart, end: idleStart });
      await stimulus.evaluate("window.lab.spinner(true)");
      await delay(2_500);
      const shotPath = resolve(dir, `reference-${cycle}.png`);
      screenshot(monitor.selector, shotPath);
      const referenceAt = performance.now();
      const refWidth = await receiver.evaluate(`window.__labQuality.setReference(${JSON.stringify(`data:image/png;base64,${readFileSync(shotPath).toString("base64")}`)})`);
      await delay(Math.max(0, idleStart + half - performance.now()));
      const idleEnd = performance.now();
      await receiver.evaluate("window.__labQuality.setReference(null)");
      windows.push({ kind: "idle", start: idleStart, end: idleEnd, referenceAt, refWidth });
      const taken = await receiver.evaluate("window.__labQuality.take()");
      if (taken.error) report.probeError = taken.error;
      quality.push(...taken.records);
      if (report.hostExit !== undefined) throw new Error(`candidate host exited: ${report.hostExit}`);
      console.log(`cycle ${cycle + 1}/${cycles}`);
    }
    const measureEnd = performance.now();

    const inWindow = (t, w) => t >= w.start && t < w.end;
    const summarize = kind => {
      const list = windows.filter(w => w.kind === kind);
      const seconds = list.reduce((sum, w) => sum + (w.end - w.start), 0) / 1000;
      const selected = frames.filter(f => list.some(w => inWindow(f.t, w)));
      const settled = kind === "idle" ? frames.filter(f => list.some(w => f.t >= w.start + SETTLE_MS && f.t < w.end)) : null;
      const settledSeconds = kind === "idle" ? list.reduce((sum, w) => sum + (w.end - w.start - SETTLE_MS), 0) / 1000 : null;
      return { seconds: +seconds.toFixed(1), frames: selected.length, keys: selected.filter(f => f.key).length,
        refines: selected.filter(f => f.refine).length, bytes: selected.reduce((s, f) => s + f.bytes, 0),
        kbps: +(selected.reduce((s, f) => s + f.bytes, 0) * 8 / 1000 / seconds).toFixed(1),
        ...(settled ? { settledKbps: +(settled.reduce((s, f) => s + f.bytes, 0) * 8 / 1000 / settledSeconds).toFixed(1),
          settledFrames: settled.length, settledRefines: settled.filter(f => f.refine).length } : {}) };
    };
    const idleWindows = windows.filter(w => w.kind === "idle");
    const idleQuality = quality.filter(q => idleWindows.some(w => q.t >= w.start + SETTLE_MS && q.t < w.end));
    const referenced = quality.filter(q => q.psnrMedian !== undefined && idleWindows.some(w => q.t >= w.referenceAt + 300 && q.t < w.end));
    const wholeFrameIdle = idleQuality.filter(q => (q.changedShare ?? 0) >= WHOLE_FRAME_SHARE);
    const measured = frames.filter(f => f.t >= measureStart && f.t < measureEnd);
    const log = [resolve(dir, "host.stdout.log"), resolve(dir, "host.stderr.log")]
      .map(path => { try { return readFileSync(path, "utf8"); } catch { return ""; } }).join("\n");
    report.result = {
      setup: { frames: setupFrames, keys: setupKeys },
      measured: { seconds: +((measureEnd - measureStart) / 1000).toFixed(1), frames: measured.length,
        keys: measured.filter(f => f.key).length, refines: measured.filter(f => f.refine).length,
        maxFrameBytes: Math.max(0, ...measured.map(f => f.bytes)) },
      sessionKeys: frames.filter(f => f.key).length,
      reading: summarize("reading"),
      idle: summarize("idle"),
      presentedChanges: quality.length,
      idleSettledWholeFrameEvents: wholeFrameIdle.length,
      idleSettledWholeFrameShares: wholeFrameIdle.slice(0, 20).map(q => +q.changedShare.toFixed(2)),
      scroll: summarizeBursts(bursts, frames, quality),
      throttle: relay.throttleStats(),
      idlePsnr: {
        samples: referenced.length,
        minMedian: percentile(referenced.map(q => q.psnrMedian), 0),
        p1Median: percentile(referenced.map(q => q.psnrMedian), 0.01),
        medianMedian: percentile(referenced.map(q => q.psnrMedian), 0.5),
        minP10: percentile(referenced.map(q => q.psnrP10), 0),
        medianP10: percentile(referenced.map(q => q.psnrP10), 0.5),
      },
      hostTotals: hostTotals(log),
      contentSeconds: contentSeconds(log),
      hostKeyRequests: log.split("\n").filter(line => line.includes("WSS v4 keyframe requested")).map(line => line.slice(0, 200)),
    };
    report.windows = windows.map(w => ({ ...w, start: Math.round(w.start - measureStart), end: Math.round(w.end - measureStart),
      referenceAt: w.referenceAt && Math.round(w.referenceAt - measureStart) }));
    report.frameTimeline = measured.map(f => [Math.round(f.t - measureStart), f.bytes, f.key ? 1 : 0, f.refine ? 1 : 0]);
    report.qualityTimeline = quality.map(q => [Math.round(q.t - measureStart), q.changedShare === undefined ? null : +q.changedShare.toFixed(3),
      q.psnrMedian === undefined ? null : +q.psnrMedian.toFixed(2), q.psnrP10 === undefined ? null : +q.psnrP10.toFixed(2)]);
    save();
    console.log(JSON.stringify({ label: values.label, ...report.result, hostKeyRequests: report.result.hostKeyRequests.length }, null, 1));
  } catch (error) {
    report.error = { message: error.message };
    process.exitCode = 1;
  } finally {
    try { await receiver?.call("Browser.close"); } catch {}
    if (cursorWas) moveCursor(cursorWas.x, cursorWas.y);
    try { await stimulus?.call("Browser.close"); } catch {}
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
