// Local file transfer v1 check on the loopback direct Stream Lab relay.
//
// Same local-only setup as stream-lab-v4-desktop.mjs: the direct relay (no
// Worker, no gate), an uninstalled candidate host with placeholder values
// instead of credentials, and a headless receiver over CDP. The host saves
// into a scratch Downloads dir and reads Send To requests from a scratch
// spool dir; it never touches the real clipboard (BUBBLEPLAY_FILE_CLIPBOARD=0).
//
//   node worker/tools/lab/stream-lab-file-transfer.mjs [--candidate-host <exe>]
//     [--label candidate] [--upload-mib 50] [--download-mib 50]
//
// Steps: browser -> PC uploads of N MiB and 1 byte (hash of the saved file
// and its Mark-of-the-Web checked on disk), then PC -> browser download of N
// MiB offered through a Send To spool request (hash checked in the page).
// A small animation window runs on the captured monitor, and the receiver's
// presented fps is sampled before and during each transfer.
import { spawn, spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { startDirectLabRelay } from "./stream-lab-direct-relay.mjs";
import { buildLabHostUrl, buildReceiverChromeArgs, buildReceiverUrl, CdpPage } from "./stream-lab-live.mjs";
import { findChrome } from "./stream-lab-runtime.mjs";

const root = resolve(import.meta.dirname, "../../..");
const delay = ms => new Promise(done => setTimeout(done, ms));
const PLACEHOLDER_CREDENTIAL = "local-lab-relay-placeholder";
const MIB = 1024 * 1024;

/** Deterministic bytes (xorshift32), generated identically here and in the page. */
const PATTERN_SOURCE = `function labPattern(size, seed) {
  const out = new Uint8Array(size);
  let x = seed >>> 0 || 1;
  for (let i = 0; i < size; i += 4) {
    x ^= x << 13; x >>>= 0; x ^= x >>> 17; x ^= x << 5; x >>>= 0;
    out[i] = x & 255; out[i + 1] = (x >>> 8) & 255; out[i + 2] = (x >>> 16) & 255; out[i + 3] = x >>> 24;
  }
  return out;
}`;
const labPattern = new Function(`${PATTERN_SOURCE}; return labPattern;`)();

const ANIMATION = `<!doctype html><title>file lab motion</title><style>html,body{margin:0;height:100%;background:#fff}
#b{position:absolute;top:40%;width:160px;height:160px;background:#1a73e8;animation:m 2s linear infinite alternate}
@keyframes m{from{left:2%}to{left:80%}}</style><div id=b></div>`;

function screenBounds(deviceName) {
  const script = `Add-Type -AssemblyName System.Windows.Forms
$s = [System.Windows.Forms.Screen]::AllScreens | Where-Object { $_.DeviceName -eq '${deviceName.replace(/'/g, "''")}' } | Select-Object -First 1
if (-not $s) { $s = [System.Windows.Forms.Screen]::PrimaryScreen }
"$($s.Bounds.X),$($s.Bounds.Y),$($s.Bounds.Width),$($s.Bounds.Height)"`;
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], { encoding: "utf8", windowsHide: true });
  const [x, y, width, height] = result.stdout.trim().split(",").map(Number);
  return { x, y, width, height };
}

const sha256 = bytes => createHash("sha256").update(bytes).digest("hex");

async function main() {
  const { values } = parseArgs({ options: {
    "candidate-host": { type: "string", default: resolve(root, "host/target/release/bubbleplay-host.exe") },
    label: { type: "string", default: "candidate" },
    "upload-mib": { type: "string", default: "50" },
    "download-mib": { type: "string", default: "50" },
    "cdp-port": { type: "string", default: "9351" },
  } });
  const uploadBytes = Number(values["upload-mib"]) * MIB;
  const downloadBytes = Number(values["download-mib"]) * MIB;
  const chrome = findChrome();
  if (!chrome) throw new Error("Chrome or Edge was not found");
  if (!existsSync(values["candidate-host"])) throw new Error(`candidate host missing: ${values["candidate-host"]}`);
  const runId = randomUUID();
  const dir = resolve(root, "artifacts/stream-lab-file-transfer", `${values.label}-${runId}`);
  const downloadsDir = resolve(dir, "downloads");
  const sendtoDir = resolve(dir, "sendto");
  const offerDir = resolve(dir, "offer");
  for (const path of [downloadsDir, sendtoDir, offerDir]) mkdirSync(path, { recursive: true });
  const report = { runId, label: values.label, startedAt: new Date().toISOString(), candidateHost: values["candidate-host"], steps: {} };
  const save = () => writeFileSync(resolve(dir, "report.json"), JSON.stringify(report, null, 2));
  const relay = await startDirectLabRelay({ port: 0, publicDir: resolve(root, "worker/public") });
  let host, receiverProcess, motionProcess, receiver;
  try {
    const out = openSync(resolve(dir, "host.stdout.log"), "a"), err = openSync(resolve(dir, "host.stderr.log"), "a");
    host = spawn(values["candidate-host"], ["--mode", "serve", "--worker", buildLabHostUrl(relay.url, runId).href], {
      cwd: resolve(root, "host"), stdio: ["ignore", out, err], windowsHide: true,
      env: { ...process.env, RUST_LOG: "info",
        BUBBLEPLAY_TOKEN: PLACEHOLDER_CREDENTIAL, BUBBLEPLAY_HOST_GATE_TOKEN: PLACEHOLDER_CREDENTIAL,
        BUBBLEPLAY_DOWNLOADS_DIR: downloadsDir, BUBBLEPLAY_SENDTO_DIR: sendtoDir, BUBBLEPLAY_FILE_CLIPBOARD: "0" },
    });
    closeSync(out); closeSync(err);
    host.once("exit", code => { report.hostExit = code; });

    const url = buildReceiverUrl(relay.url, "wss", { labRun: runId, adaptive: true });
    const port = Number(values["cdp-port"]);
    const args = buildReceiverChromeArgs(resolve(dir, "receiver-profile"), port, "about:blank", { startMinimized: false });
    receiverProcess = spawn(chrome, ["--headless=new", ...args], { stdio: "ignore", windowsHide: true });
    receiver = await new CdpPage(port).open();
    await receiver.call("Emulation.setDeviceMetricsOverride", { width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false });
    await receiver.navigate(url.href);
    const evaluate = expression => receiver.evaluate(expression);
    const until = async (expression, timeoutMs, label = expression) => {
      const deadline = performance.now() + timeoutMs;
      while (performance.now() < deadline) {
        if (await evaluate(expression).catch(() => false)) return;
        await delay(100);
      }
      throw new Error(`timed out waiting for ${label}`);
    };
    await until("window.__bubbleplayDiagnostics?.snapshot?.()?.framesDrawn > 0", 30_000, "first frame");
    await until("window.__bubbleplayFiles?.capability?.() === true", 15_000, "fileTransfer capability");

    const monitors = await evaluate("window.__bubbleplayDiagnostics?.snapshot?.()?.signaling?.monitors ?? []");
    const monitor = monitors.find(m => m.selected) ?? monitors[0];
    report.monitor = monitor;
    if (monitor) {
      const bounds = screenBounds(monitor.selector);
      const animationPath = resolve(dir, "motion.html");
      writeFileSync(animationPath, ANIMATION);
      motionProcess = spawn(chrome, [`--user-data-dir=${resolve(dir, "motion-profile")}`, "--no-first-run",
        `--window-position=${bounds.x + 40},${bounds.y + 30}`, "--window-size=900,600", `--app=${pathToFileURL(animationPath).href}`],
      { stdio: "ignore" });
      await delay(2_500);
    }

    const framesDrawn = () => evaluate("window.__bubbleplayDiagnostics.snapshot().framesDrawn");
    const fpsOver = async ms => {
      const before = await framesDrawn();
      const start = performance.now();
      await delay(ms);
      return +(((await framesDrawn()) - before) / ((performance.now() - start) / 1000)).toFixed(1);
    };
    /** Run a transfer started by `startExpression` (returns row ids) and sample fps while it runs. */
    const transfer = async startExpression => {
      const ids = await evaluate(startExpression);
      const samples = [];
      let rows;
      for (;;) {
        const before = await framesDrawn();
        const t0 = performance.now();
        await delay(500);
        rows = await evaluate(`window.__bubbleplayFiles.client.transfers.filter(r => ${JSON.stringify(ids)}.includes(r.id))`);
        const running = rows.some(r => !["done", "failed", "cancelled"].includes(r.state));
        if (running) samples.push(((await framesDrawn()) - before) / ((performance.now() - t0) / 1000));
        if (!running) break;
      }
      const row = rows[0];
      const seconds = Math.max(0.001, (row.finishedAt - row.startedAt) / 1000);
      return {
        state: row.state, error: row.error, bytes: row.size,
        seconds: +seconds.toFixed(2), mbps: +((row.size * 8) / seconds / 1e6).toFixed(1),
        fpsDuring: samples.length ? +(samples.reduce((a, b) => a + b, 0) / samples.length).toFixed(1) : null,
        fpsDuringMin: samples.length ? +Math.min(...samples).toFixed(1) : null,
      };
    };
    await evaluate(`${PATTERN_SOURCE}; window.labPattern = labPattern; true`);
    const savedFile = name => {
      const path = resolve(downloadsDir, name);
      if (!existsSync(path)) return { exists: false, entries: readdirSync(downloadsDir) };
      let zone = null;
      try { zone = readFileSync(`${path}:Zone.Identifier`, "utf8"); } catch {}
      return { exists: true, sha256: sha256(readFileSync(path)), motw: zone, leftovers: readdirSync(downloadsDir).filter(entry => entry.includes("bppartial")) };
    };

    // 1. Upload N MiB.
    report.fpsBefore = await fpsOver(5_000);
    const upload = await transfer(`window.__bubbleplayFiles.client.upload([new File([labPattern(${uploadBytes}, 7)], "lab-upload.bin")], "save")`);
    const uploadDisk = savedFile("lab-upload.bin");
    report.steps.upload = { ...upload, expectedSha256: sha256(labPattern(uploadBytes, 7)), disk: uploadDisk,
      hashOk: uploadDisk.sha256 === sha256(labPattern(uploadBytes, 7)) };
    save();

    // 2. Upload 1 byte.
    const tiny = await transfer(`window.__bubbleplayFiles.client.upload([new File([new Uint8Array([42])], "lab-one-byte.txt")], "save")`);
    const tinyDisk = savedFile("lab-one-byte.txt");
    report.steps.uploadOneByte = { ...tiny, disk: tinyDisk, hashOk: tinyDisk.sha256 === sha256(Buffer.from([42])) };
    save();

    // 3. Download N MiB offered through Send To.
    const offered = resolve(offerDir, "lab-download.bin");
    const downloadPattern = labPattern(downloadBytes, 11);
    writeFileSync(offered, downloadPattern);
    writeFileSync(resolve(sendtoDir, `${Date.now()}-lab.json`), JSON.stringify({ protocol: 1, createdMs: Date.now(), paths: [offered] }));
    await until("window.__bubbleplayFiles.client.offer?.items?.length === 1", 15_000, "file offer");
    report.offer = await evaluate("window.__bubbleplayFiles.client.offer");
    report.fpsBeforeDownload = await fpsOver(3_000);
    const download = await transfer(`(async () => {
      const { Sha256 } = await import("/file-transfer.js");
      const client = window.__bubbleplayFiles.client;
      window.__labDownloadHash = null;
      return client.download(client.offer.items, async () => {
        const hash = new Sha256();
        return { write: async bytes => { hash.update(bytes); }, close: async () => {
          window.__labDownloadHash = Array.from(hash.finish(), b => b.toString(16).padStart(2, "0")).join("");
        }, abort: async () => {} };
      });
    })()`);
    const pageHash = await evaluate("window.__labDownloadHash");
    report.steps.download = { ...download, pageSha256: pageHash, expectedSha256: sha256(downloadPattern), hashOk: pageHash === sha256(downloadPattern) };
    report.fpsAfter = await fpsOver(3_000);
    const log = readFileSync(resolve(dir, "host.stdout.log"), "utf8") + readFileSync(resolve(dir, "host.stderr.log"), "utf8");
    report.hostFileLines = log.split("\n").filter(line => /file (upload|download)|file transfer|file offer|file socket/.test(line)).map(line => line.slice(0, 300)).slice(-30);
    report.ok = report.steps.upload.hashOk && report.steps.uploadOneByte.hashOk && report.steps.download.hashOk &&
      Boolean(uploadDisk.motw?.includes("ZoneId=3")) && uploadDisk.leftovers.length === 0;
    console.log(JSON.stringify({ ok: report.ok, fpsBefore: report.fpsBefore, fpsBeforeDownload: report.fpsBeforeDownload, fpsAfter: report.fpsAfter,
      upload: report.steps.upload, uploadOneByte: report.steps.uploadOneByte, download: report.steps.download }, null, 1));
    if (!report.ok) process.exitCode = 1;
  } catch (error) {
    report.error = { message: error.message };
    console.error(error);
    process.exitCode = 1;
  } finally {
    try { await receiver?.call("Browser.close"); } catch {}
    for (const child of [motionProcess, receiverProcess, host]) {
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
