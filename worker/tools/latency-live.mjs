#!/usr/bin/env node
// Dependency-free CDP soak and software glass-to-glass collector.
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";
import { spawn, spawnSync } from "node:child_process";

const root = resolve(import.meta.dirname, "..");
const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 && args[index + 1] ? args[index + 1] : fallback;
};
const has = (name) => args.includes(name);
const envFile = resolve(root, ".dev.vars");
const token = process.env.BUBBLEPLAY_TOKEN || (existsSync(envFile)
  ? (readFileSync(envFile, "utf8").match(/^TOKEN\s*=\s*["']?([^\r\n"']+)/m)?.[1] || "") : "");
if (!token) throw new Error("set BUBBLEPLAY_TOKEN or worker/.dev.vars TOKEN");

const baseUrl = option("--url", process.env.BUBBLEPLAY_URL || "https://bubbleplay-signal.your-subdomain.workers.dev/");
const seconds = Math.max(5, Number(option("--seconds", process.env.BUBBLEPLAY_SOAK_SECONDS || 30)));
const stallMs = Math.max(0, Number(option("--stall-ms", 750)));
const glassToGlass = has("--glass-to-glass");
const decoderPath = option("--decode-path", process.env.BUBBLEPLAY_DECODER_PATH || "");
const port = Number(option("--cdp-port", process.env.BUBBLEPLAY_CDP_PORT || 19333));
const output = resolve(root, "..", "artifacts", "latency", `${glassToGlass ? "benchmark" : "live"}-${Date.now()}.json`);
const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));

function findChrome() {
  const candidates = [
    process.env.BUBBLEPLAY_CHROME,
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
    "chrome",
  ].filter(Boolean);
  return candidates.find((candidate) => candidate === "chrome" || existsSync(candidate));
}

function percentile(values, fraction) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * fraction) - 1)];
}

async function cdpSocket(endpoint) {
  const ws = new WebSocket(endpoint);
  await new Promise((resolveOpen, reject) => { ws.onopen = resolveOpen; ws.onerror = reject; });
  let id = 0;
  const pending = new Map();
  ws.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    const resolver = pending.get(message.id);
    if (resolver) { pending.delete(message.id); resolver(message); }
  };
  return {
    call(method, params = {}) {
      const requestId = ++id;
      ws.send(JSON.stringify({ id: requestId, method, params }));
      return new Promise((resolveCall, reject) => {
        pending.set(requestId, (message) => message.error ? reject(new Error(message.error.message)) : resolveCall(message.result));
      });
    },
    close: () => ws.close(),
  };
}

async function evaluate(cdp, expression) {
  const response = await cdp.call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
  if (response.exceptionDetails) throw new Error(response.exceptionDetails.text || "Runtime.evaluate failed");
  return response.result.value;
}

async function pageTarget(cdpPort) {
  for (let retry = 0; retry < 100; retry++) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${cdpPort}/json/list`)).json();
      const page = targets.find((target) => target.type === "page");
      if (page?.webSocketDebuggerUrl) return page;
    } catch { /* Chrome is still starting */ }
    await sleep(100);
  }
  throw new Error("Chromium page CDP endpoint did not become available");
}

function terminateTree(child) {
  if (!child?.pid) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    child.kill("SIGTERM");
  }
}

async function main() {
  const chrome = findChrome();
  if (!chrome) throw new Error("Chrome/Edge not found; set BUBBLEPLAY_CHROME");
  const url = new URL(baseUrl);
  url.searchParams.set("token", token);
  url.searchParams.set("diag", "1");
  if (decoderPath) url.searchParams.set("decoderPath", decoderPath);

  const receiverProfile = mkdtempSync(resolve(tmpdir(), "bubbleplay-receiver-"));
  let patternProfile = null;
  let patternChild = null;
  const receiverArguments = [
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${receiverProfile}`,
    "--no-first-run",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--autoplay-policy=no-user-gesture-required",
    "about:blank",
  ];
  if (glassToGlass) {
    // Headless Chromium is heavily decode/rAF-bound on this Windows GPU. A
    // headed receiver keeps the real hardware path, stays off the captured
    // primary display, and continues rendering while the source kiosk is on
    // top because background/occlusion throttling is disabled above.
    receiverArguments.splice(receiverArguments.length - 1, 0,
      "--window-position=-2000,0",
      "--window-size=1280,720",
      "--force-device-scale-factor=1");
  } else {
    receiverArguments.splice(receiverArguments.length - 1, 0, "--headless=new");
  }
  const receiverChild = spawn(chrome, receiverArguments, { stdio: "ignore" });

  if (glassToGlass) {
    patternProfile = mkdtempSync(resolve(tmpdir(), "bubbleplay-pattern-"));
    const patternUrl = pathToFileURL(resolve(root, "public", "timestamp-pattern.html")).href;
    patternChild = spawn(chrome, [
      `--user-data-dir=${patternProfile}`,
      "--no-first-run",
      "--disable-background-timer-throttling",
      "--disable-backgrounding-occluded-windows",
      "--disable-renderer-backgrounding",
      "--force-device-scale-factor=1",
      "--kiosk",
      patternUrl,
    ], { stdio: "ignore" });
    await sleep(1500);
  }

  const report = {
    schemaVersion: 1,
    startedAt: new Date().toISOString(),
    seconds,
    url: url.origin + url.pathname,
    glassToGlassEnabled: glassToGlass,
    decoderPath: decoderPath || "transform",
    samples: [],
  };

  try {
    const page = await pageTarget(port);
    const cdp = await cdpSocket(page.webSocketDebuggerUrl);
    await cdp.call("Page.enable");
    await cdp.call("Runtime.enable");
    let ready = null;
    for (let attempt = 0; attempt < 3; attempt++) {
      const attemptUrl = new URL(url);
      attemptUrl.searchParams.set("testAttempt", String(attempt));
      await cdp.call("Page.navigate", { url: attemptUrl.href });
      for (let retry = 0; retry < 40; retry++) {
        const raw = await evaluate(cdp, "JSON.stringify(window.__bubbleplayDiagnostics?.snapshot?.() ?? null)");
        ready = raw ? JSON.parse(raw) : null;
        if (ready && ready.ice === "connected" && Number.isFinite(ready.renderedTimestamp)) break;
        await sleep(250);
      }
      if (ready && ready.ice === "connected" && Number.isFinite(ready.renderedTimestamp)) break;
      // A stale offer can make the host recycle immediately after a Worker
      // deployment. Navigating away closes the old client before re-offering
      // to the freshly re-registered host.
      await cdp.call("Page.navigate", { url: "about:blank" });
      await sleep(750);
    }
    if (!ready || ready.ice !== "connected" || !Number.isFinite(ready.renderedTimestamp)) {
      throw new Error(`receiver did not reach ICE connected with rendered diagnostics (last=${JSON.stringify(ready)})`);
    }

    const started = Date.now();
    const stallAt = started + Math.min(5_000, Math.max(2_000, seconds * 250));
    let stallInjectedAt = null;
    let preStall = null;
    while (Date.now() - started < seconds * 1000) {
      if (stallMs && stallInjectedAt === null && Date.now() >= stallAt) {
        const raw = await evaluate(cdp, "JSON.stringify(window.__bubbleplayDiagnostics.snapshot())");
        preStall = JSON.parse(raw);
        stallInjectedAt = Date.now();
        await evaluate(cdp, `window.__bubbleplayDiagnostics.injectMainThreadStall(${JSON.stringify(stallMs)}); true`);
      }
      const raw = await evaluate(cdp, "JSON.stringify(window.__bubbleplayDiagnostics?.snapshot?.() ?? null)");
      const snapshot = raw ? JSON.parse(raw) : null;
      if (snapshot) report.samples.push({ elapsedMs: Date.now() - started, ...snapshot });
      await sleep(250);
    }

    const rendered = report.samples.filter((sample) => Number.isFinite(sample.renderedTimestamp));
    const stallEndElapsed = stallInjectedAt === null ? null : stallInjectedAt - started + stallMs;
    const recovery = stallEndElapsed === null ? null : rendered.find((sample) =>
      sample.elapsedMs >= stallEndElapsed &&
      sample.renderedTimestamp !== preStall.renderedTimestamp &&
      sample.renderedAgeMs <= 100
    );
    const recoveryEnd = recovery?.elapsedMs ?? (stallEndElapsed === null ? null : stallEndElapsed + 2_000);
    const steady = rendered.filter((sample) =>
      sample.worker?.recovering !== true &&
      (stallEndElapsed === null || sample.elapsedMs < stallInjectedAt - started || sample.elapsedMs >= recoveryEnd));
    const displayedAges = steady.map((sample) => sample.renderedAgeMs).filter(Number.isFinite);
    // A frozen canvas repeats the same barcode. Count each source frame once
    // so cached pixels cannot manufacture a passing latency distribution.
    const seenBarcodeSequences = new Set();
    const glassToGlassSamples = report.samples.flatMap((sample) => {
      const value = sample.glassToGlass;
      if (sample.worker?.recovering === true || !value || !Number.isFinite(value.ageMs) || value.ageMs < 0 || value.ageMs >= 10_000 ||
          seenBarcodeSequences.has(value.sequence)) return [];
      seenBarcodeSequences.add(value.sequence);
      return [value.ageMs];
    });
    report.metrics = {
      renderedSamples: rendered.length,
      addedLagP50Ms: percentile(displayedAges, 0.50),
      addedLagP95Ms: percentile(displayedAges, 0.95),
      addedLagMaxMs: displayedAges.length ? Math.max(...displayedAges) : null,
      samplesOver100MsOutsideRecovery: displayedAges.filter((age) => age > 100).length,
      injectedMainThreadStallMs: stallMs,
      recoveryMs: recovery && stallEndElapsed !== null ? recovery.elapsedMs - stallEndElapsed : null,
      generationChangedDuringStall: stallEndElapsed === null ? null : rendered.some((sample) => sample.elapsedMs >= stallInjectedAt - started && sample.elapsedMs <= stallEndElapsed && sample.generation !== preStall.generation),
      glassToGlassSamples: glassToGlassSamples.length,
      glassToGlassP50Ms: percentile(glassToGlassSamples, 0.50),
      glassToGlassP95Ms: percentile(glassToGlassSamples, 0.95),
      glassToGlassP99Ms: percentile(glassToGlassSamples, 0.99),
      glassToGlassMaxMs: glassToGlassSamples.length ? Math.max(...glassToGlassSamples) : null,
    };
    const latencyPass = rendered.length > 0 &&
      report.metrics.samplesOver100MsOutsideRecovery === 0 &&
      (!stallMs || (report.metrics.recoveryMs !== null && report.metrics.recoveryMs <= 2_000));
    const benchmarkPass = !glassToGlass ||
      (glassToGlassSamples.length >= 10 && report.metrics.glassToGlassP95Ms <= 150);
    report.result = latencyPass && benchmarkPass ? "pass" : "fail";
    cdp.close();
  } catch (error) {
    report.result = "unavailable";
    report.error = String(error);
  } finally {
    report.finishedAt = new Date().toISOString();
    mkdirSync(dirname(output), { recursive: true });
    writeFileSync(output, JSON.stringify(report, null, 2));
    terminateTree(receiverChild);
    terminateTree(patternChild);
    await sleep(250);
    try { rmSync(receiverProfile, { recursive: true, force: true, maxRetries: 3 }); } catch {}
    if (patternProfile) try { rmSync(patternProfile, { recursive: true, force: true, maxRetries: 3 }); } catch {}
  }
  console.log(JSON.stringify({ report: output, result: report.result, metrics: report.metrics || null }));
  if (report.result !== "pass") process.exitCode = 1;
}

await main();
