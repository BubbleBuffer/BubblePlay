import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(SCRIPT_DIR, "..", "..");
const ARTIFACT_ROOT = join(REPO_ROOT, "artifacts", "stream-lab");
const FONT_ROOT = join(REPO_ROOT, "worker", "public", "fonts");

function parseArguments(argv) {
  const runs = [];
  let output = join(ARTIFACT_ROOT, "quiescent-detail-report");
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--output") output = resolve(argv[++index]);
    else if (argument === "--run") {
      const [kind, label, runId] = String(argv[++index] ?? "").split(",");
      if (!kind || !label || !runId) throw new Error("--run requires kind,label,run-id");
      if (!new Set(["control", "candidate", "reference"]).has(kind))
        throw new Error(`unsupported run kind: ${kind}`);
      runs.push({ kind, label, runId });
    } else throw new Error(`unknown argument: ${argument}`);
  }
  if (!runs.length) throw new Error("provide at least one --run kind,label,run-id");
  return { runs, output };
}

function finite(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function deltaRate(currentBytes, previousBytes, currentMs, previousMs) {
  const bytes = finite(currentBytes);
  const before = finite(previousBytes);
  const elapsed = finite(currentMs) - finite(previousMs);
  return bytes !== null && before !== null && elapsed > 0
    ? Math.max(0, bytes - before) * 8 / elapsed / 1_000
    : null;
}

function frameTrace(event) {
  return event?.frameTrace?.trace ?? event?.frameTrace ?? null;
}

function readJson(path) {
  return existsSync(path) ? JSON.parse(readFileSync(path, "utf8")) : null;
}

function readTelemetry(path) {
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8").split(/\r?\n/).filter(Boolean).map((line, index) => {
    try { return JSON.parse(line); }
    catch (error) { throw new Error(`${basename(path)}:${index + 1}: ${error.message}`); }
  });
}

function aggregateFromMetrics(document) {
  if (!document) return null;
  if (document.result === "complete" && finite(document.presentedFps) !== null) return document;
  if (document.error?.detail?.metrics) return document.error.detail.metrics;
  return null;
}

function errorFromMetrics(document) {
  if (!document || document.result === "complete") return null;
  const error = document.error ?? null;
  return error ? {
    code: error.code ?? "run-failed",
    message: error.message ?? "The run did not complete.",
    stage: error.detail?.stage ?? null,
    reasons: error.detail?.continuity?.reasons ?? error.detail?.reasons ?? [],
  } : { code: document.result ?? "run-failed", message: "The run did not complete.", stage: null, reasons: [] };
}

function hostEvents(path, firstEpochMs) {
  if (!existsSync(path) || firstEpochMs === null) return [];
  const events = [];
  let expired = 0;
  let dependencyLosses = 0;
  for (const line of readFileSync(path, "utf8").split(/\r?\n/)) {
    const timestamp = line.match(/^(\S+)/)?.[1];
    const epochMs = timestamp ? Date.parse(timestamp) : NaN;
    if (!Number.isFinite(epochMs)) continue;
    const t = (epochMs - firstEpochMs) / 1_000;
    if (t < -1) continue;
    const reset = line.match(/client_event=wss-recovery-reset client_detail=(\{.*\})$/);
    if (reset) {
      const detail = JSON.parse(reset[1]);
      events.push({ t, type: "reset", label: `Decoder reset · ${detail.reason ?? "recovery"}` });
      continue;
    }
    const keyframe = line.match(/client_event=wss-recovery-keyframe client_detail=(\{.*\})$/);
    if (keyframe) {
      const detail = JSON.parse(keyframe[1]);
      events.push({ t, type: "recovery", label: `Recovery keyframe · sequence ${detail.sequence ?? "?"}` });
      continue;
    }
    const delivery = line.match(/WSS v3 delivery window .*expired_frames=(\d+) dependency_losses=(\d+).*recovery_state="([^"]+)"/);
    if (delivery) {
      const nextExpired = Number(delivery[1]);
      const nextLosses = Number(delivery[2]);
      if (nextExpired > expired)
        events.push({ t, type: "expiry", label: `${nextExpired - expired} frame expiry${nextExpired - expired === 1 ? "" : "ies"}` });
      if (nextLosses > dependencyLosses)
        events.push({ t, type: "loss", label: `${nextLosses - dependencyLosses} dependency loss` });
      expired = nextExpired;
      dependencyLosses = nextLosses;
    }
  }
  return events;
}

function summarize(run, aggregate, error, samples) {
  const last = samples.at(-1) ?? {};
  const hybrid = aggregate?.hostHybridActivity ?? {};
  return {
    ...run,
    result: error ? "failed" : "complete",
    error,
    presentedFps: finite(aggregate?.presentedFps) ?? finite(last.fps),
    latencyP95Ms: finite(aggregate?.latencyP95Ms),
    frameGapMaxMs: finite(aggregate?.frameGapMaxMs) ?? finite(last.maxFrameGap),
    exactFrameRatio: finite(aggregate?.exactFrameRatio),
    decoderResets: finite(aggregate?.decoderResets) ?? finite(last.resyncs) ?? 0,
    dependencyLosses: finite(aggregate?.dependencyLosses) ?? finite(last.dependencyLosses) ?? 0,
    pngBytes: finite(hybrid.pngBytes),
    sidecarBytes: finite(hybrid.sidecarBytes),
    upserts: finite(hybrid.upserts),
    invalidations: finite(hybrid.invalidations),
    suppressions: finite(hybrid.suppressions),
    reactivations: finite(hybrid.reactivations),
    pngBytesAvoided: finite(hybrid.pngBytesAvoided),
    vmafP5: finite(aggregate?.vmafP5),
    ocrCer: finite(aggregate?.ocrCer),
    worstTileSsim: finite(aggregate?.worstTileSsim),
  };
}

function loadRun(run) {
  const directory = join(ARTIFACT_ROOT, run.runId);
  const telemetry = readTelemetry(join(directory, "telemetry.ndjson"));
  const performance = telemetry.filter(event => event.phase === "performance" && finite(event.clock?.epochMs) !== null);
  const firstEpochMs = performance.length ? Number(performance[0].clock.epochMs) : null;
  const samples = performance.map((event, index) => {
    const previous = performance[index - 1];
    const trace = frameTrace(event);
    const network = trace?.network ?? {};
    const decode = trace?.decode ?? {};
    const presentation = trace?.presentation ?? {};
    const hybrid = event.benchStatus?.hybridActivity ?? {};
    const sequences = Array.isArray(event.wssMedia?.highestSequenceByLane)
      ? event.wssMedia.highestSequenceByLane.map(Number).filter(Number.isFinite)
      : [];
    const encodeCount = finite(event.benchStatus?.encodeCount);
    return {
      t: (Number(event.clock.epochMs) - firstEpochMs) / 1_000,
      phaseElapsed: finite(event.elapsedMs) === null ? null : Number(event.elapsedMs) / 1_000,
      fps: finite(event.fps),
      renderedAgeMs: finite(event.glassToGlass?.ageMs) ?? finite(event.renderedAgeMs),
      sourceToPresentedMs: finite(presentation.sourceToPresentedMs),
      frameGapMs: finite(event.frameGap),
      maxFrameGap: finite(event.maxFrameGap),
      assemblyMs: finite(network.reconstructedAtMs) !== null && finite(network.firstShardAtMs) !== null
        ? Number(network.reconstructedAtMs) - Number(network.firstShardAtMs) : null,
      decodeMs: finite(decode.decodedAtMs) !== null && finite(decode.queuedAtMs) !== null
        ? Number(decode.decodedAtMs) - Number(decode.queuedAtMs) : null,
      encodeMeanMs: encodeCount > 0 ? finite(event.benchStatus?.encodeUsTotal) / encodeCount / 1_000 : null,
      encodeMaxMs: finite(event.benchStatus?.encodeUsMax) === null ? null : Number(event.benchStatus.encodeUsMax) / 1_000,
      offeredMbps: finite(event.benchStatus?.wssPayloadBytes) !== null && finite(event.benchStatus?.windowDurationMs) > 0
        ? Number(event.benchStatus.wssPayloadBytes) * 8 / Number(event.benchStatus.windowDurationMs) / 1_000 : null,
      deliveredMbps: previous ? deltaRate(event.wssMedia?.receivedBytes, previous.wssMedia?.receivedBytes,
        event.clock.epochMs, previous.clock?.epochMs) : null,
      queued: finite(event.queued),
      laneSpan: sequences.length ? Math.max(...sequences) - Math.min(...sequences) : null,
      resyncs: finite(event.resyncs) ?? 0,
      expiredFrames: finite(event.wssMedia?.expiredFrames) ?? 0,
      dependencyLosses: finite(event.wssMedia?.dependencyLosses) ?? 0,
      pngKiB: finite(hybrid.pngBytes) === null ? null : Number(hybrid.pngBytes) / 1_024,
      sidecarKiB: finite(hybrid.sidecarBytes) === null ? null : Number(hybrid.sidecarBytes) / 1_024,
      upserts: finite(hybrid.upserts),
      invalidations: finite(hybrid.invalidations),
      suppressions: finite(hybrid.cache?.s ?? hybrid.suppressions),
      reactivations: finite(hybrid.cache?.a ?? hybrid.reactivations),
    };
  });
  const metrics = readJson(join(directory, "metrics.json"));
  const aggregate = aggregateFromMetrics(metrics);
  const error = errorFromMetrics(metrics);
  const events = hostEvents(join(directory, "candidate-host.stderr.log"), firstEpochMs);
  if (error) {
    const finalT = Math.max(0, ...samples.map(sample => sample.t), ...events.map(event => event.t));
    events.push({ t: finalT, type: "failure", label: `${error.code}${error.stage ? ` · ${error.stage}` : ""}` });
  }
  return { ...run, directory, samples, events, summary: summarize(run, aggregate, error, samples) };
}

function htmlDocument(data) {
  const serialized = JSON.stringify(data).replaceAll("<", "\\u003c");
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>BubblePlay · WSS telemetry report</title>
<style>
@font-face{font-family:Atkinson;src:url("fonts/atkinson-hyperlegible-next.woff2") format("woff2");font-weight:200 800;font-display:swap}
@font-face{font-family:Bricolage;src:url("fonts/bricolage-grotesque.woff2") format("woff2");font-weight:200 800;font-stretch:75% 100%;font-display:swap}
@font-face{font-family:Fragment;src:url("fonts/fragment-mono.woff2") format("woff2");font-weight:400;font-display:swap}
:root{color-scheme:dark;--ink:#282a2f;--duvet:#2f3138;--surface:#30333a;--raised:#3b3e45;--milk:#f3efe7;--cream:#c6baac;--thread:#575b64;--thread-strong:#999da6;--apricot:#f0a064;--paw:#dda39d;--moss:#8da18f;--danger:#e5b1aa;--focus:#7ab7ff}
*{box-sizing:border-box}html{background:var(--ink)}body{margin:0;color:var(--milk);background:var(--ink);font:450 15px/1.5 Atkinson,"Segoe UI",sans-serif}::selection{color:var(--ink);background:var(--apricot)}
button{font:720 12px/1.3 Atkinson,sans-serif;color:var(--milk);background:var(--surface);border:1px solid var(--thread);border-radius:7px 11px 6px 9px;padding:9px 12px;cursor:pointer}button:hover{background:var(--raised);border-color:var(--thread-strong)}button[aria-pressed="true"]{color:var(--ink);background:var(--apricot);border-color:var(--apricot)}button:focus-visible{outline:2px solid var(--focus);outline-offset:3px}
.shell{min-height:100vh}.top{position:sticky;top:0;z-index:4;display:flex;align-items:center;justify-content:space-between;gap:24px;padding:14px clamp(18px,4vw,56px);background:#282a2ff2;border-bottom:1px solid var(--thread)}.brand{display:flex;align-items:center;gap:10px;font:660 18px/1 Bricolage,sans-serif;letter-spacing:-.025em}.signal{width:24px;height:24px}.signal path{fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:square;stroke-linejoin:bevel}.signal .live{stroke:var(--apricot)}.context{color:var(--cream);font-size:12px}.context code{font-family:Fragment,monospace;color:var(--milk)}
main{width:min(1500px,100%);margin:auto;padding:clamp(28px,5vw,72px) clamp(18px,4vw,56px) 72px}.hero{display:grid;grid-template-columns:minmax(0,1.25fr) minmax(280px,.75fr);gap:48px;align-items:end;padding-bottom:36px}.hero h1{max-width:820px;margin:0;font:620 clamp(42px,6vw,86px)/.9 Bricolage,sans-serif;font-stretch:82%;letter-spacing:-.04em}.hero h1 span{color:var(--apricot)}.hero p{max-width:62ch;margin:0;color:var(--cream);font-size:16px}.status-line{display:flex;flex-wrap:wrap;gap:8px;margin-top:18px}.status{display:inline-flex;align-items:center;gap:7px;padding:5px 9px;border:1px solid var(--thread);border-radius:999px;font:400 10px/1.4 Fragment,monospace}.status::before{content:"";width:7px;height:7px;background:var(--moss);border:1px solid currentColor}.status.attention::before{background:var(--danger)}
.run-controls{display:flex;flex-wrap:wrap;gap:8px;padding:18px 0 24px;border-top:1px solid var(--thread)}.run-controls .run-button{display:inline-flex;align-items:center;gap:8px}.swatch{width:12px;height:3px;background:var(--run-color)}.run-button.control .swatch{height:0;border-top:2px dashed var(--run-color)}
.ledger{border-top:1px solid var(--thread-strong)}.chart-row{display:grid;grid-template-columns:minmax(190px,.27fr) minmax(0,1fr);min-height:270px;border-bottom:1px solid var(--thread)}.chart-copy{padding:26px 28px 24px 0}.chart-copy h2{margin:0 0 8px;font:620 25px/1.05 Bricolage,sans-serif;letter-spacing:-.025em}.chart-copy p{max-width:30ch;margin:0;color:var(--cream);font-size:13px}.chart-copy code{font:400 10px/1.6 Fragment,monospace;color:var(--apricot)}.chart{position:relative;min-width:0;padding:14px 0 4px 24px;border-left:1px solid var(--thread)}svg.plot{display:block;width:100%;height:250px;overflow:visible}.grid line{stroke:var(--thread);stroke-width:1;vector-effect:non-scaling-stroke}.axis text{fill:var(--cream);font:10px Fragment,monospace}.axis-label{fill:var(--cream);font:10px Fragment,monospace}.series{fill:none;stroke-width:2.1;vector-effect:non-scaling-stroke}.series.control{stroke-dasharray:8 6}.point{stroke:var(--ink);stroke-width:1.2;vector-effect:non-scaling-stroke}.threshold{stroke:var(--moss);stroke-width:1;stroke-dasharray:3 5;vector-effect:non-scaling-stroke}.event-line{stroke:var(--danger);stroke-width:1;stroke-dasharray:2 4;vector-effect:non-scaling-stroke}.event-mark{fill:var(--danger);stroke:var(--ink);stroke-width:1;vector-effect:non-scaling-stroke}.empty{display:grid;place-items:center;height:240px;color:var(--cream)}
.evidence{padding-top:48px}.evidence h2{margin:0 0 18px;font:620 34px/1 Bricolage,sans-serif;letter-spacing:-.03em}.table-wrap{overflow:auto;border-top:1px solid var(--thread-strong);border-bottom:1px solid var(--thread)}table{width:100%;border-collapse:collapse;font-variant-numeric:tabular-nums}th,td{padding:12px 14px;border-bottom:1px solid var(--thread);text-align:right;white-space:nowrap}th:first-child,td:first-child{text-align:left;position:sticky;left:0;background:var(--ink)}th{color:var(--cream);font:400 10px/1.4 Fragment,monospace}.run-name{display:flex;align-items:center;gap:9px;font-weight:720}.result-failed{color:var(--danger)}.mono{font-family:Fragment,monospace;font-size:11px}.note{margin:18px 0 0;color:var(--cream);font-size:12px}
@media(max-width:780px){.top{position:static;align-items:flex-start}.context{display:none}.hero{grid-template-columns:1fr;gap:22px}.chart-row{grid-template-columns:1fr}.chart-copy{padding-right:0}.chart{padding-left:0;border-left:0}.run-controls{overflow-x:auto;flex-wrap:nowrap}.run-controls button{white-space:nowrap}main{padding-top:32px}}
@media(prefers-reduced-motion:no-preference){.series{stroke-dasharray:3000;stroke-dashoffset:3000;animation:draw .65s cubic-bezier(.16,1,.3,1) forwards}@keyframes draw{to{stroke-dashoffset:0}}}
@media(prefers-reduced-motion:reduce){*{scroll-behavior:auto!important;animation:none!important;transition:none!important}}
</style>
</head>
<body>
<div class="shell">
  <header class="top"><div class="brand"><svg class="signal" viewBox="0 0 24 24" aria-hidden="true"><path d="M9 4H4v16h16v-5M15 4h5v5"/><path class="live" d="m10 14 4-4m-4 0h4v4"/></svg>BubblePlay</div><div class="context">WSS protocol <code>3</code> · <code>16 lanes / 15+1</code> · <code>6 Mbps cap</code></div></header>
  <main>
    <section class="hero"><div><h1>The stream, <span>over time.</span></h1></div><div><p>Raw Stream Lab samples for quiescent-detail controls and candidates. Failed runs remain visible; reset, expiry, loss, and recovery events are marked instead of averaged away.</p><div class="status-line"><span class="status">immutable Worker v7</span><span class="status">mouse-neutral</span><span class="status attention">failures retained</span></div></div></section>
    <nav class="run-controls" aria-label="Visible runs" id="run-controls"></nav>
    <section class="ledger" id="charts"></section>
    <section class="evidence"><h2>Run ledger</h2><div class="table-wrap"><table><thead><tr><th>Run</th><th>Result</th><th>FPS</th><th>Latency p95</th><th>Max gap</th><th>Resets</th><th>Losses</th><th>PNG</th><th>Sidecar</th><th>Upserts</th><th>Suppression</th><th>Reactivation</th><th>VMAF p5</th><th>OCR CER</th></tr></thead><tbody id="run-table"></tbody></table></div><p class="note">Time-series values come from <span class="mono">telemetry.ndjson</span>. Aggregate quality and cache totals come from each run’s <span class="mono">metrics.json</span>. Host recovery markers come from <span class="mono">candidate-host.stderr.log</span>.</p></section>
  </main>
</div>
<script>
const REPORT = ${serialized};
const colors=["#8da18f","#c6baac","#e5b1aa","#f0a064","#999da6","#dda39d","#7ab7ff","#de945f"];
const charts=[
  {key:"fps",title:"Presented cadence",copy:"Rolling browser presentation rate. The acceptance seam is 27 FPS.",unit:"FPS",threshold:27,thresholdLabel:"27 FPS gate"},
  {key:"sourceToPresentedMs",fallback:"renderedAgeMs",title:"Source to presentation",copy:"Frame age from host capture through browser presentation.",unit:"ms"},
  {key:"frameGapMs",fallback:"maxFrameGap",title:"Frame continuity",copy:"Observed presentation gap. Recovery events are pinned to the same clock.",unit:"ms",events:true},
  {key:"assemblyMs",title:"Network assembly",copy:"First arriving shard to a reconstructable 15-of-16 frame.",unit:"ms"},
  {key:"deliveredMbps",fallback:"offeredMbps",title:"WSS wire rate",copy:"Delivered media rate from receiver byte deltas; host-offered rate fills missing samples.",unit:"Mbps",threshold:6,thresholdLabel:"6 Mbps cap"},
  {key:"encodeMeanMs",fallback:"encodeMaxMs",title:"Host encode work",copy:"Mean NVENC work per benchmark telemetry window.",unit:"ms"},
  {key:"pngKiB",title:"PNG detail work",copy:"PNG bytes generated in each reported host window.",unit:"KiB / window"},
  {key:"sidecarKiB",title:"Total enhancement sidecar",copy:"Hybrid envelope overhead per host telemetry window.",unit:"KiB / window"},
  {key:"upserts",title:"Tile replacement churn",copy:"PNG tile upserts per host window. The cache candidate should replace this with cheap suppress/reactivate operations.",unit:"ops / window"},
  {key:"suppressions",title:"Cache suppression",copy:"Immediate stale-safety operations awaiting the one-frame-late GPU verdict.",unit:"ops / window"},
];
const visible=new Set(REPORT.runs.map(run=>run.runId));
REPORT.runs.forEach((run,index)=>run.color=colors[index%colors.length]);
const fmt=(value,digits=1)=>Number.isFinite(value)?Number(value).toFixed(digits):"—";
const bytes=value=>Number.isFinite(value)?value>=1048576?(value/1048576).toFixed(2)+" MiB":(value/1024).toFixed(1)+" KiB":"—";
const esc=value=>String(value??"").replace(/[&<>\"]/g,char=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[char]));
function path(points,x,y){return points.map((point,index)=>(index?"L":"M")+x(point.t).toFixed(2)+" "+y(point.value).toFixed(2)).join(" ")}
function metricPoints(run,chart){return run.samples.map(sample=>({t:sample.t,value:Number.isFinite(sample[chart.key])?sample[chart.key]:sample[chart.fallback]})).filter(point=>Number.isFinite(point.value))}
function renderChart(chart){
  const active=REPORT.runs.filter(run=>visible.has(run.runId));
  const series=active.map(run=>({run,points:metricPoints(run,chart)})).filter(item=>item.points.length);
  if(!series.length)return '<div class="chart-row"><div class="chart-copy"><h2>'+chart.title+'</h2><p>'+chart.copy+'</p></div><div class="chart"><div class="empty">No samples for visible runs.</div></div></div>';
  const all=series.flatMap(item=>item.points); const eventTimes=chart.events?active.flatMap(run=>run.events.map(event=>event.t)):[];
  const xMax=Math.max(1,...all.map(point=>point.t),...eventTimes); let yMin=Math.min(0,...all.map(point=>point.value)); let yMax=Math.max(chart.threshold??0,...all.map(point=>point.value));
  if(yMax===yMin)yMax=yMin+1; const pad=(yMax-yMin)*.1; yMax+=pad; if(yMin<0)yMin-=pad;
  const W=1000,H=240,L=64,R=18,T=18,B=34; const x=value=>L+value/xMax*(W-L-R); const y=value=>T+(yMax-value)/(yMax-yMin)*(H-T-B);
  const grid=Array.from({length:5},(_,index)=>{const value=yMin+(yMax-yMin)*index/4;return '<g><line x1="'+L+'" x2="'+(W-R)+'" y1="'+y(value)+'" y2="'+y(value)+'"/><text x="'+(L-10)+'" y="'+(y(value)+3)+'" text-anchor="end">'+fmt(value,value>=100?0:1)+'</text></g>'}).join("");
  const ticks=Array.from({length:5},(_,index)=>{const value=xMax*index/4;return '<text x="'+x(value)+'" y="'+(H-10)+'" text-anchor="middle">'+fmt(value,0)+'s</text>'}).join("");
  const threshold=Number.isFinite(chart.threshold)?'<line class="threshold" x1="'+L+'" x2="'+(W-R)+'" y1="'+y(chart.threshold)+'" y2="'+y(chart.threshold)+'"><title>'+chart.thresholdLabel+'</title></line>':"";
  const events=chart.events?active.flatMap(run=>run.events.filter(event=>event.t>=0).map(event=>'<g><line class="event-line" x1="'+x(event.t)+'" x2="'+x(event.t)+'" y1="'+T+'" y2="'+(H-B)+'"/><rect class="event-mark" x="'+(x(event.t)-4)+'" y="'+(T-4)+'" width="8" height="8"><title>'+esc(run.label+" · "+event.label+" · "+fmt(event.t,1)+"s")+'</title></rect></g>')).join(""):"";
  const lines=series.map(({run,points})=>'<g><path class="series '+run.kind+'" style="stroke:'+run.color+'" d="'+path(points,x,y)+'"/>'+points.filter((_,index)=>index%Math.max(1,Math.floor(points.length/14))===0||index===points.length-1).map(point=>'<circle class="point" cx="'+x(point.t)+'" cy="'+y(point.value)+'" r="3" style="fill:'+run.color+'"><title>'+esc(run.label+" · "+fmt(point.value,2)+" "+chart.unit+" · "+fmt(point.t,1)+"s")+'</title></circle>').join("")+'</g>').join("");
  return '<div class="chart-row"><div class="chart-copy"><h2>'+chart.title+'</h2><p>'+chart.copy+'</p><code>'+chart.unit+'</code></div><div class="chart"><svg class="plot" viewBox="0 0 '+W+' '+H+'" role="img" aria-label="'+chart.title+' over time"><g class="grid axis">'+grid+'</g><g class="axis">'+ticks+'</g>'+threshold+events+lines+'</svg></div></div>';
}
function render(){
  document.getElementById("run-controls").innerHTML=REPORT.runs.map(run=>'<button class="run-button '+run.kind+'" style="--run-color:'+run.color+'" data-run="'+run.runId+'" aria-pressed="'+visible.has(run.runId)+'"><span class="swatch"></span>'+esc(run.label)+(run.summary.result==="failed"?' · failed':'')+'</button>').join("");
  document.querySelectorAll("[data-run]").forEach(button=>button.addEventListener("click",()=>{visible.has(button.dataset.run)?visible.delete(button.dataset.run):visible.add(button.dataset.run);render()}));
  document.getElementById("charts").innerHTML=charts.map(renderChart).join("");
  document.getElementById("run-table").innerHTML=REPORT.runs.map(run=>{const s=run.summary;return '<tr><td><span class="run-name"><span class="swatch" style="--run-color:'+run.color+'"></span>'+esc(run.label)+'</span><span class="mono">'+run.runId.slice(0,8)+'</span></td><td class="'+(s.result==="failed"?'result-failed':'')+'">'+esc(s.result==="failed"?s.error?.code:"complete")+'</td><td>'+fmt(s.presentedFps,2)+'</td><td>'+fmt(s.latencyP95Ms,0)+'</td><td>'+fmt(s.frameGapMaxMs,0)+'</td><td>'+fmt(s.decoderResets,0)+'</td><td>'+fmt(s.dependencyLosses,0)+'</td><td>'+bytes(s.pngBytes)+'</td><td>'+bytes(s.sidecarBytes)+'</td><td>'+fmt(s.upserts,0)+'</td><td>'+fmt(s.suppressions,0)+'</td><td>'+fmt(s.reactivations,0)+'</td><td>'+fmt(s.vmafP5,2)+'</td><td>'+fmt(s.ocrCer,3)+'</td></tr>'}).join("");
}
render();
</script>
</body>
</html>`;
}

const options = parseArguments(process.argv.slice(2));
const runs = options.runs.map(loadRun);
const generatedAt = new Date().toISOString();
const report = { schemaVersion: 1, generatedAt, runs };
mkdirSync(options.output, { recursive: true });
mkdirSync(join(options.output, "fonts"), { recursive: true });
for (const font of ["atkinson-hyperlegible-next.woff2", "bricolage-grotesque.woff2", "fragment-mono.woff2"])
  copyFileSync(join(FONT_ROOT, font), join(options.output, "fonts", font));
writeFileSync(join(options.output, "data.json"), `${JSON.stringify(report, null, 2)}\n`);
writeFileSync(join(options.output, "index.html"), htmlDocument(report));
console.log(JSON.stringify({ output: options.output, generatedAt, runs: runs.map(run => run.summary) }, null, 2));
