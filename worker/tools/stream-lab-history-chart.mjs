import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { extractHistoryMeasurements } from "./stream-lab-history-data.mjs";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(SCRIPT_DIR, "..", "..");
const DEFAULT_ARTIFACT_ROOT = join(REPO_ROOT, "artifacts", "stream-lab");
const FONT_ROOT = join(REPO_ROOT, "worker", "public", "fonts");

function parseArguments(argv) {
  let artifactRoot = DEFAULT_ARTIFACT_ROOT;
  let output = join(DEFAULT_ARTIFACT_ROOT, "history-report");
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--artifact-root") artifactRoot = resolve(argv[++index]);
    else if (argument === "--output") output = resolve(argv[++index]);
    else throw new Error(`unknown argument: ${argument}`);
  }
  return { artifactRoot, output };
}

function readJson(path) {
  if (!existsSync(path)) return null;
  try { return JSON.parse(readFileSync(path, "utf8")); }
  catch (error) { return { __parseError: error.message }; }
}

function finite(value) {
  if (value === null || value === undefined || value === "") return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function errorFrom(metrics, diagnostics) {
  const error = metrics?.error ?? diagnostics ?? null;
  if (!error) return null;
  return {
    code: String(error.code ?? metrics?.result ?? "run-failed"),
    stage: String(error.detail?.stage ?? "") || null,
    reasons: error.detail?.continuity?.reasons ?? error.detail?.reasons ?? [],
  };
}

function attemptResult(metrics, diagnostics) {
  if (metrics?.__parseError) return "unreadable";
  if (metrics?.result) return String(metrics.result);
  if (diagnostics?.code) return "failed";
  return "recorded";
}

function loadDataset(artifactRoot) {
  const directories = readdirSync(artifactRoot, { withFileTypes: true }).filter(entry => entry.isDirectory());
  const measurements = [];
  const attempts = [];
  let manifestFiles = 0;
  let metricsFiles = 0;
  let failureFiles = 0;

  for (const entry of directories) {
    const directory = join(artifactRoot, entry.name);
    const manifestPath = join(directory, "manifest.json");
    const metricsPath = join(directory, "metrics.json");
    const failurePath = join(directory, "failure-diagnostics.json");
    const hasManifest = existsSync(manifestPath);
    const hasMetrics = existsSync(metricsPath);
    const hasFailure = existsSync(failurePath);
    if (!hasManifest && !hasMetrics && !hasFailure) continue;
    if (hasManifest) manifestFiles += 1;
    if (hasMetrics) metricsFiles += 1;
    if (hasFailure) failureFiles += 1;

    const manifest = readJson(manifestPath) ?? {};
    const metrics = readJson(metricsPath);
    const diagnostics = readJson(failurePath);
    const candidateFiles = [manifestPath, metricsPath, failurePath].filter(existsSync);
    const endedAtMs = Math.max(...candidateFiles.map(path => statSync(path).mtimeMs));
    const startedAtParsed = Date.parse(manifest.startedAt ?? "");
    const startedAtMs = Number.isFinite(startedAtParsed) ? startedAtParsed : endedAtMs;
    const runMeasurements = metrics && !metrics.__parseError
      ? extractHistoryMeasurements({ manifest, metrics, runId: entry.name, endedAtMs })
      : [];
    measurements.push(...runMeasurements);
    const error = errorFrom(metrics, diagnostics);
    attempts.push({
      runId: entry.name,
      atMs: startedAtMs,
      endedAtMs,
      result: attemptResult(metrics, diagnostics),
      error,
      command: String(manifest.command ?? "unknown"),
      transport: String(manifest.transport ?? metrics?.transport ?? "") || null,
      codec: String(manifest.codec ?? metrics?.codec ?? "") || null,
      measurements: runMeasurements.length,
      experiments: [...new Set(runMeasurements.map(point => point.experimentId).filter(Boolean))],
      configurations: [...new Set(runMeasurements.map(point => point.configurationId).filter(Boolean))],
    });
  }

  measurements.sort((left, right) => left.atMs - right.atMs || left.pointId.localeCompare(right.pointId));
  attempts.sort((left, right) => left.atMs - right.atMs || left.runId.localeCompare(right.runId));
  const measuredArtifacts = attempts.filter(attempt => attempt.measurements > 0).length;
  const failedAttempts = attempts.filter(attempt => !["complete", "recorded", "running"].includes(attempt.result)).length;
  const stages = Object.fromEntries([...new Set(measurements.map(point => point.stage))]
    .sort().map(stage => [stage, measurements.filter(point => point.stage === stage).length]));
  const experimentCounts = [...measurements.reduce((counts, point) => {
    counts.set(point.experimentId, (counts.get(point.experimentId) ?? 0) + 1);
    return counts;
  }, new Map())].sort((left, right) => right[1] - left[1]);
  const startedAtMs = Math.min(...attempts.map(attempt => attempt.atMs));
  const endedAtMs = Math.max(...attempts.map(attempt => attempt.endedAtMs));

  return {
    schemaVersion: 1,
    generatedAt: new Date().toISOString(),
    summary: {
      artifactDirectories: directories.length,
      recordedAttempts: attempts.length,
      manifestFiles,
      metricsFiles,
      failureFiles,
      measuredArtifacts,
      measurementPasses: measurements.length,
      failedAttempts,
      comparablePasses: measurements.filter(point => point.comparable).length,
      startedAtMs,
      endedAtMs,
      stages,
      experimentCounts,
    },
    measurements,
    attempts,
  };
}

function csvCell(value) {
  const text = value === null || value === undefined ? "" : String(value);
  return /[",\r\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

function historyCsv(measurements) {
  const fields = [
    "atMs", "runId", "path", "stage", "result", "command", "experimentId", "configurationId",
    "implementationVariant", "transport", "codec", "topology", "requestedBitrateMbps", "comparable",
    "presentedFps", "latencyP95Ms", "sourceToPresentedP95Ms", "frameGapMaxMs", "encodeP95Ms",
    "wireBitrateMbps", "vmafP5", "ocrCer", "worstTileSsim", "edgeFidelity", "exactFrameRatio",
    "gpuEncoderMeanPct", "hostCpuPct",
  ];
  return `${fields.join(",")}\n${measurements.map(point => fields.map(field => csvCell(point[field])).join(",")).join("\n")}\n`;
}

function htmlDocument(report) {
  const serialized = JSON.stringify(report).replaceAll("<", "\\u003c");
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>BubblePlay · Stream Lab history</title>
<style>
@font-face{font-family:Atkinson;src:url("fonts/atkinson-hyperlegible-next.woff2") format("woff2");font-weight:200 800;font-display:swap}
@font-face{font-family:Bricolage;src:url("fonts/bricolage-grotesque.woff2") format("woff2");font-weight:200 800;font-stretch:75% 100%;font-display:swap}
@font-face{font-family:Fragment;src:url("fonts/fragment-mono.woff2") format("woff2");font-weight:400;font-display:swap}
:root{color-scheme:dark;--ink:#282a2f;--surface:#30333a;--raised:#3b3e45;--milk:#f3efe7;--cream:#c6baac;--thread:#575b64;--thread-strong:#999da6;--apricot:#f0a064;--paw:#dda39d;--moss:#8da18f;--danger:#e5b1aa;--focus:#7ab7ff}
*{box-sizing:border-box}html{background:var(--ink)}body{margin:0;color:var(--milk);background:var(--ink);font:450 15px/1.5 Atkinson,"Segoe UI",sans-serif;font-variant-numeric:tabular-nums}::selection{color:var(--ink);background:var(--apricot)}
a{color:inherit;text-decoration-color:var(--apricot);text-underline-offset:3px}a:focus-visible,button:focus-visible,input:focus-visible{outline:2px solid var(--focus);outline-offset:3px}
button{font:720 12px/1.3 Atkinson,sans-serif;color:var(--milk);background:var(--surface);border:1px solid var(--thread);border-radius:7px 11px 6px 9px;padding:9px 12px;cursor:pointer}button:hover{background:var(--raised);border-color:var(--thread-strong)}button[aria-pressed="true"]{color:var(--ink);background:var(--apricot);border-color:var(--apricot)}
.shell{min-height:100vh}.top{position:sticky;top:0;z-index:4;display:flex;align-items:center;justify-content:space-between;gap:24px;padding:14px clamp(18px,4vw,56px);background:#282a2ff2;border-bottom:1px solid var(--thread)}.brand{display:flex;align-items:center;gap:10px;font:660 18px/1 Bricolage,sans-serif;letter-spacing:-.025em}.signal{width:24px;height:24px}.signal path{fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:square;stroke-linejoin:bevel}.signal .live{stroke:var(--apricot)}.context{color:var(--cream);font-size:12px}.context code,.mono{font-family:Fragment,monospace;color:var(--milk)}
main{width:min(1500px,100%);margin:auto;padding:clamp(30px,5vw,72px) clamp(18px,4vw,56px) 80px}.hero{display:grid;grid-template-columns:minmax(0,1.15fr) minmax(300px,.85fr);gap:56px;align-items:end;padding-bottom:38px}.hero h1{max-width:850px;margin:0;font:620 clamp(42px,6vw,86px)/.9 Bricolage,sans-serif;font-stretch:82%;letter-spacing:-.04em}.hero h1 span{color:var(--apricot)}.hero p{max-width:64ch;margin:0;color:var(--cream);font-size:16px}.hero strong{color:var(--milk)}
.evidence-seam{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));border-top:1px solid var(--thread-strong);border-bottom:1px solid var(--thread)}.datum{min-width:0;padding:18px 20px 18px 0}.datum+.datum{padding-left:20px;border-left:1px solid var(--thread)}.datum dt{color:var(--cream);font:400 10px/1.4 Fragment,monospace}.datum dd{margin:4px 0 0;font:620 clamp(25px,3vw,38px)/1 Bricolage,sans-serif;letter-spacing:-.025em}.datum small{display:block;margin-top:7px;color:var(--cream);font-size:11px}
.scope{display:flex;flex-wrap:wrap;align-items:center;gap:8px;padding:20px 0 12px}.scope-label{margin-right:5px;color:var(--cream);font:400 10px/1.4 Fragment,monospace}.stage-key{display:inline-block;width:9px;height:9px;margin-right:7px;background:var(--stage-color);border:1px solid currentColor}.downloads{margin-left:auto;display:flex;gap:14px;font-size:12px}.chart-filter{display:flex;align-items:center;gap:14px;padding:0 0 20px}.chart-filter label{font:720 12px/1.3 Atkinson,sans-serif}.chart-filter input{width:min(520px,100%);height:42px;padding:9px 12px;color:var(--milk);background:var(--surface);border:1px solid var(--thread);border-radius:6px 10px 6px 8px;font:450 13px/1.4 Atkinson,sans-serif}.chart-filter-count{margin-left:auto;color:var(--cream);font:400 10px/1.4 Fragment,monospace}
.activity{display:grid;grid-template-columns:minmax(210px,.28fr) minmax(0,1fr);min-height:225px;border-top:1px solid var(--thread-strong);border-bottom:1px solid var(--thread)}.activity-copy{padding:28px 30px 24px 0}.activity-copy h2,.chart-copy h2{margin:0 0 8px;font:620 25px/1.05 Bricolage,sans-serif;letter-spacing:-.025em}.activity-copy p,.chart-copy p{max-width:34ch;margin:0;color:var(--cream);font-size:13px}.activity-plot,.chart{min-width:0;padding:14px 0 4px 24px;border-left:1px solid var(--thread)}
.section-head{display:flex;align-items:end;justify-content:space-between;gap:24px;padding:52px 0 16px}.section-head h2{margin:0;font:620 clamp(31px,4vw,48px)/.95 Bricolage,sans-serif;letter-spacing:-.035em}.section-head p{max-width:58ch;margin:0;color:var(--cream);font-size:13px}
.ledger{border-top:1px solid var(--thread-strong)}.chart-row{display:grid;grid-template-columns:minmax(210px,.28fr) minmax(0,1fr);min-height:285px;border-bottom:1px solid var(--thread)}.chart-copy{padding:28px 30px 24px 0}.chart-copy code{display:block;margin-top:9px;font:400 10px/1.6 Fragment,monospace;color:var(--apricot)}.trend-note{margin-top:16px!important;color:var(--milk)!important;font:400 10px/1.5 Fragment,monospace!important}.plot{display:block;width:100%;height:260px;overflow:visible}.grid line{stroke:var(--thread);stroke-width:1;vector-effect:non-scaling-stroke}.axis text{fill:var(--cream);font:10px Fragment,monospace}.point{stroke:var(--ink);stroke-width:1;vector-effect:non-scaling-stroke;opacity:.72}.point.calibration{fill:transparent;stroke:var(--cream);opacity:.44}.point.rollup{fill:var(--moss)}.point.comparison{fill:var(--paw)}.point.failure-snapshot{fill:var(--danger)}.point.run,.point.repetition,.point.pass{fill:var(--apricot)}.point.clipped{stroke:var(--danger);stroke-width:2}.trend{fill:none;stroke:var(--milk);stroke-width:2.2;vector-effect:non-scaling-stroke}.threshold{stroke:var(--moss);stroke-width:1;stroke-dasharray:3 5;vector-effect:non-scaling-stroke}.empty{display:grid;place-items:center;height:250px;color:var(--cream)}
.attempts{padding-top:48px}.attempt-tools{display:flex;align-items:center;gap:16px;margin-bottom:14px}.attempt-tools label{font:720 12px/1.3 Atkinson,sans-serif}.attempt-tools input{width:min(520px,100%);height:42px;padding:9px 12px;color:var(--milk);background:var(--surface);border:1px solid var(--thread);border-radius:6px 10px 6px 8px;font:450 13px/1.4 Atkinson,sans-serif}.attempt-count{margin-left:auto;color:var(--cream);font:400 10px/1.4 Fragment,monospace}.table-wrap{overflow:auto;border-top:1px solid var(--thread-strong);border-bottom:1px solid var(--thread);max-height:620px}table{width:100%;border-collapse:collapse}th,td{padding:11px 13px;border-bottom:1px solid var(--thread);text-align:left;white-space:nowrap}th{position:sticky;top:0;z-index:2;color:var(--cream);background:var(--ink);font:400 10px/1.4 Fragment,monospace}th:first-child,td:first-child{position:sticky;left:0;background:var(--ink)}td{font-size:12px}.failed{color:var(--danger)}.note{margin:18px 0 0;color:var(--cream);font-size:12px}
@media(max-width:780px){.top{position:static}.context{display:none}.hero{grid-template-columns:1fr;gap:22px}.evidence-seam{grid-template-columns:1fr 1fr}.datum:nth-child(3){padding-left:0;border-left:0;border-top:1px solid var(--thread)}.datum:nth-child(4){border-top:1px solid var(--thread)}.downloads{width:100%;margin:8px 0 0}.activity,.chart-row{grid-template-columns:1fr}.activity-copy,.chart-copy{padding-right:0}.activity-plot,.chart{padding-left:0;border-left:0}.scope{overflow-x:auto;flex-wrap:nowrap}.scope button{white-space:nowrap}.chart-filter{align-items:stretch;flex-direction:column}.chart-filter input{width:100%}.chart-filter-count{margin:0}.section-head{align-items:start;flex-direction:column}.attempt-tools{align-items:stretch;flex-direction:column}.attempt-count{margin:0}main{padding-top:34px}}
@media(prefers-reduced-motion:no-preference){.trend{stroke-dasharray:3000;stroke-dashoffset:3000;animation:draw .65s cubic-bezier(.16,1,.3,1) forwards}@keyframes draw{to{stroke-dashoffset:0}}}
@media(prefers-reduced-motion:reduce){*{animation:none!important;transition:none!important}}
</style>
</head>
<body>
<div class="shell">
  <header class="top"><div class="brand"><svg class="signal" viewBox="0 0 24 24" aria-hidden="true"><path d="M9 4H4v16h16v-5M15 4h5v5"/><path class="live" d="m10 14 4-4m-4 0h4v4"/></svg>BubblePlay</div><div class="context">Stream Lab history · <code>2026-08-23 → 2026-09-01</code></div></header>
  <main>
    <section class="hero"><div><h1>Every measured <span>frame of progress.</span></h1></div><div><p>This is the full local Stream Lab history, not one recent cohort. It separates recorded attempts from KPI-bearing passes and preserves calibration, repetition, comparison, rollup, and failure-snapshot evidence.</p></div></section>
    <dl class="evidence-seam">
      <div class="datum"><dt>Recorded attempts</dt><dd id="attempt-total"></dd><small>manifests, metrics, or failure evidence</small></div>
      <div class="datum"><dt>Measurement passes</dt><dd id="pass-total"></dd><small>noise summaries excluded</small></div>
      <div class="datum"><dt>Metric-bearing artifacts</dt><dd id="measured-total"></dd><small>of <span id="metrics-total"></span> metrics files</small></div>
      <div class="datum"><dt>Unsuccessful attempts</dt><dd id="failed-total"></dd><small>kept out of trend medians</small></div>
    </dl>
    <div class="scope" id="stage-controls"><span class="scope-label">VISIBLE EVIDENCE</span></div>
    <div class="chart-filter"><label for="chart-filter">Filter charts</label><input id="chart-filter" type="search" placeholder="e.g. main10, dirty-qp-map, 15+1, quiescent-detail"><span class="chart-filter-count" id="chart-filter-count"></span></div>
    <section class="activity"><div class="activity-copy"><h2>Experiment activity</h2><p>Daily recorded attempts. Apricot reached at least one KPI pass; danger marks attempts that stopped before or during evaluation.</p></div><div class="activity-plot" id="activity"></div></section>
    <div class="section-head"><h2>Pipeline evolution</h2><p>Each dot is one recorded measurement object. The pale line is a 21-pass rolling median of visible non-calibration evidence. Extreme tails are pinned to a robust chart edge and remain readable in tooltips.</p></div>
    <section class="ledger" id="charts"></section>
    <section class="attempts"><div class="section-head"><h2>Attempt ledger</h2><p>A benchmark pass is evidence, not automatically a distinct code change. Search by run, experiment, configuration, command, or failure code.</p></div><div class="attempt-tools"><label for="attempt-filter">Filter history</label><input id="attempt-filter" type="search" placeholder="e.g. 15+1, main10, dirty-qp-map, wss-cohort-lost"><span class="attempt-count" id="attempt-count"></span></div><div class="table-wrap"><table><thead><tr><th>Started</th><th>Result</th><th>Passes</th><th>Experiment / configuration</th><th>Command</th><th>Run</th><th>Failure</th></tr></thead><tbody id="attempt-table"></tbody></table></div><p class="note">Generated from local immutable artifacts. <a href="history-data.json">Download normalized JSON</a> or <a href="history-measurements.csv">measurement CSV</a>. No production route or service state is involved.</p></section>
  </main>
</div>
<script>
const REPORT=${serialized};
const stageMeta={run:{label:"direct runs",color:"#f0a064"},repetition:{label:"repetitions",color:"#f0a064"},calibration:{label:"calibration",color:"#c6baac"},comparison:{label:"recovery comparisons",color:"#dda39d"},rollup:{label:"campaign rollups",color:"#8da18f"},"failure-snapshot":{label:"failure snapshots",color:"#e5b1aa"},pass:{label:"other passes",color:"#999da6"}};
const charts=[
 {key:"presentedFps",title:"Presented cadence",copy:"Browser-presented frame rate across recorded runs.",unit:"FPS",threshold:27,digits:1},
 {key:"latencyP95Ms",title:"End-to-end latency p95",copy:"The historical glass-to-glass latency field used by Stream Lab.",unit:"ms",digits:0},
 {key:"sourceToPresentedP95Ms",title:"Source to presentation p95",copy:"Trace-derived capture-to-browser presentation latency, available after trace telemetry landed.",unit:"ms",digits:0},
 {key:"frameGapMaxMs",title:"Worst frame gap",copy:"Maximum continuity gap per measurement. Recovery outliers remain pinned and labeled.",unit:"ms",digits:0},
 {key:"encodeP95Ms",title:"Encode work p95",copy:"Host encode p95, including the early tuning and resource experiments.",unit:"ms",digits:1},
 {key:"wireBitrateMbps",fallback:"deliveredWireBitrateMbps",title:"WSS wire rate",copy:"Measured delivered wire rate after shaping and parity overhead.",unit:"Mbps",threshold:6,digits:2},
 {key:"vmafP5",title:"VMAF p5",copy:"Worst-tail perceptual quality from quality-bearing passes.",unit:"score",digits:2,higher:true},
 {key:"ocrCer",title:"OCR character error",copy:"Text reconstruction error; lower is better.",unit:"CER",digits:3},
 {key:"worstTileSsim",title:"Worst-tile SSIM",copy:"Localized worst-region structural similarity.",unit:"SSIM",digits:3,higher:true},
 {key:"gpuEncoderMeanPct",title:"GPU encoder load",copy:"Mean NVENC engine utilization reported during the pass.",unit:"%",digits:1},
];
const visibleStages=new Set(Object.keys(REPORT.summary.stages));
let comparableOnly=false;
let profileQuery="";
const esc=value=>String(value??"").replace(/[&<>\"]/g,char=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[char]));
const fmt=(value,digits=1)=>Number.isFinite(value)?Number(value).toFixed(digits):"—";
const median=values=>{const sorted=values.filter(Number.isFinite).sort((a,b)=>a-b);if(!sorted.length)return null;const middle=Math.floor(sorted.length/2);return sorted.length%2?sorted[middle]:(sorted[middle-1]+sorted[middle])/2};
const quantile=(sorted,q)=>{if(!sorted.length)return null;const at=(sorted.length-1)*q;const below=Math.floor(at),above=Math.ceil(at);return below===above?sorted[below]:sorted[below]+(sorted[above]-sorted[below])*(at-below)};
const dateLabel=ms=>new Intl.DateTimeFormat("en",{month:"short",day:"numeric",hour:"2-digit",minute:"2-digit",hour12:false}).format(new Date(ms));
const dayLabel=ms=>new Intl.DateTimeFormat("en",{month:"short",day:"numeric"}).format(new Date(ms));
function metricValue(point,chart){return Number.isFinite(point[chart.key])?point[chart.key]:point[chart.fallback]}
function pointMatches(point){return !profileQuery||[point.runId,point.command,point.experimentId,point.configurationId,point.topology,point.implementationVariant].join(" ").toLowerCase().includes(profileQuery)}
function visiblePoints(chart){return REPORT.measurements.filter(point=>visibleStages.has(point.stage)&&(!comparableOnly||point.comparable)&&pointMatches(point)&&Number.isFinite(metricValue(point,chart)))}
function rollingMedian(points,chart){const eligible=points.filter(point=>!new Set(["calibration","rollup","failure-snapshot"]).has(point.stage)).sort((a,b)=>a.atMs-b.atMs);return eligible.map((point,index)=>({atMs:point.atMs,value:median(eligible.slice(Math.max(0,index-20),index+1).map(item=>metricValue(item,chart)))}))}
function renderActivity(){
 const days=new Map();for(const attempt of REPORT.attempts){const key=new Date(attempt.atMs).toISOString().slice(0,10);const bucket=days.get(key)??{key,measured:0,failed:0,total:0};bucket.total++;if(attempt.measurements)bucket.measured++;if(!["complete","recorded","running"].includes(attempt.result))bucket.failed++;days.set(key,bucket)}
 const entries=[...days.values()].sort((a,b)=>a.key.localeCompare(b.key));const max=Math.max(1,...entries.map(item=>item.total));const W=1000,H=200,L=54,R=18,T=18,B=34,slot=(W-L-R)/entries.length,bar=Math.min(52,slot*.58);const y=value=>T+(max-value)/max*(H-T-B);
 const grid=[0,.5,1].map(f=>'<g><line x1="'+L+'" x2="'+(W-R)+'" y1="'+y(max*f)+'" y2="'+y(max*f)+'"/><text x="'+(L-9)+'" y="'+(y(max*f)+3)+'" text-anchor="end">'+Math.round(max*f)+'</text></g>').join("");
 const bars=entries.map((item,index)=>{const x=L+slot*index+(slot-bar)/2;const measuredH=(H-B)-y(item.measured),failedH=(H-B)-y(item.failed);return '<g><rect x="'+x+'" y="'+y(item.measured)+'" width="'+bar+'" height="'+measuredH+'" fill="#f0a064"><title>'+esc(item.key+' · '+item.measured+' metric-bearing / '+item.total+' attempts')+'</title></rect><rect x="'+(x+bar*.68)+'" y="'+y(item.failed)+'" width="'+(bar*.32)+'" height="'+failedH+'" fill="#e5b1aa"><title>'+esc(item.key+' · '+item.failed+' unsuccessful attempts')+'</title></rect><text x="'+(x+bar/2)+'" y="'+(H-10)+'" text-anchor="middle">'+esc(dayLabel(Date.parse(item.key+'T12:00:00Z')))+'</text></g>'}).join("");
 document.getElementById("activity").innerHTML='<svg class="plot" viewBox="0 0 '+W+' '+H+'" role="img" aria-label="Daily Stream Lab activity"><g class="grid axis">'+grid+'</g><g class="axis">'+bars+'</g></svg>';
}
function renderChart(chart){
 const points=visiblePoints(chart).sort((a,b)=>a.atMs-b.atMs);if(!points.length)return '<div class="chart-row"><div class="chart-copy"><h2>'+chart.title+'</h2><p>'+chart.copy+'</p></div><div class="chart"><div class="empty">No matching measurements.</div></div></div>';
 const values=points.map(point=>metricValue(point,chart)).sort((a,b)=>a-b);let yMin=quantile(values,.02),yMax=quantile(values,.98);if(yMin===yMax){yMin-=1;yMax+=1}const pad=(yMax-yMin)*.08;yMin-=pad;yMax+=pad;if(chart.key==="presentedFps")yMin=Math.max(0,yMin);if(chart.key==="ocrCer"||chart.key==="worstTileSsim"||chart.key==="exactFrameRatio")yMin=Math.max(0,yMin);
 const xMin=REPORT.summary.startedAtMs,xMax=REPORT.summary.endedAtMs,W=1000,H=250,L=66,R=18,T=18,B=35,x=value=>L+(value-xMin)/(xMax-xMin)*(W-L-R),y=value=>T+(yMax-Math.min(yMax,Math.max(yMin,value)))/(yMax-yMin)*(H-T-B);const grid=Array.from({length:5},(_,index)=>{const value=yMin+(yMax-yMin)*index/4;return '<g><line x1="'+L+'" x2="'+(W-R)+'" y1="'+y(value)+'" y2="'+y(value)+'"/><text x="'+(L-10)+'" y="'+(y(value)+3)+'" text-anchor="end">'+fmt(value,chart.digits)+'</text></g>'}).join("");const ticks=Array.from({length:5},(_,index)=>{const value=xMin+(xMax-xMin)*index/4;return '<text x="'+x(value)+'" y="'+(H-10)+'" text-anchor="middle">'+esc(dayLabel(value))+'</text>'}).join("");
 const threshold=Number.isFinite(chart.threshold)&&chart.threshold>=yMin&&chart.threshold<=yMax?'<line class="threshold" x1="'+L+'" x2="'+(W-R)+'" y1="'+y(chart.threshold)+'" y2="'+y(chart.threshold)+'"><title>'+chart.threshold+' '+chart.unit+' reference</title></line>':"";
 const dots=points.map(point=>{const value=metricValue(point,chart),clipped=value<yMin||value>yMax;const radius=point.stage==="rollup"?3.8:point.stage==="calibration"?2.4:3;const title=point.experimentId+' · '+fmt(value,chart.digits)+' '+chart.unit+' · '+dateLabel(point.atMs)+' · '+point.stage+' · '+point.runId.slice(0,8);return '<a href="../'+encodeURIComponent(point.runId)+'/metrics.json"><circle class="point '+point.stage+(clipped?' clipped':'')+'" cx="'+x(point.atMs)+'" cy="'+y(value)+'" r="'+radius+'"><title>'+esc(title+(clipped?' · clipped in view':''))+'</title></circle></a>'}).join("");
 const trend=rollingMedian(points,chart);const trendPath=trend.map((point,index)=>(index?'L':'M')+x(point.atMs).toFixed(2)+' '+y(point.value).toFixed(2)).join(' ');const clippedCount=points.filter(point=>{const value=metricValue(point,chart);return value<yMin||value>yMax}).length;const primary=points.filter(point=>point.stage!=="calibration"&&point.stage!=="rollup");const early=median(primary.slice(0,Math.min(21,primary.length)).map(point=>metricValue(point,chart))),recent=median(primary.slice(-Math.min(21,primary.length)).map(point=>metricValue(point,chart)));const delta=Number.isFinite(early)&&Number.isFinite(recent)?recent-early:null;
 return '<div class="chart-row"><div class="chart-copy"><h2>'+chart.title+'</h2><p>'+chart.copy+'</p><code>'+points.length+' passes · '+chart.unit+(clippedCount?' · '+clippedCount+' tails pinned':'')+'</code><p class="trend-note">early median '+fmt(early,chart.digits)+' → recent '+fmt(recent,chart.digits)+(Number.isFinite(delta)?' · Δ '+(delta>=0?'+':'')+fmt(delta,chart.digits):'')+'</p></div><div class="chart"><svg class="plot" viewBox="0 0 '+W+' '+H+'" role="img" aria-label="'+chart.title+' over experiment history"><g class="grid axis">'+grid+'</g><g class="axis">'+ticks+'</g>'+threshold+dots+'<path class="trend" d="'+trendPath+'"><title>21-pass rolling median</title></path></svg></div></div>';
}
function renderStageControls(){
 const host=document.getElementById("stage-controls");const stageButtons=Object.entries(REPORT.summary.stages).map(([stage,count])=>{const meta=stageMeta[stage]??{label:stage,color:"#999da6"};return '<button data-stage="'+esc(stage)+'" aria-pressed="'+visibleStages.has(stage)+'" style="--stage-color:'+meta.color+'"><span class="stage-key"></span>'+esc(meta.label)+' · '+count+'</button>'}).join("");host.innerHTML='<span class="scope-label">VISIBLE EVIDENCE</span>'+stageButtons+'<button id="comparable" aria-pressed="'+comparableOnly+'">1080p30 · 6 Mbps only</button><span class="downloads"><a href="history-data.json">JSON</a><a href="history-measurements.csv">CSV</a></span>';host.querySelectorAll('[data-stage]').forEach(button=>button.addEventListener('click',()=>{visibleStages.has(button.dataset.stage)?visibleStages.delete(button.dataset.stage):visibleStages.add(button.dataset.stage);render()}));host.querySelector('#comparable').addEventListener('click',()=>{comparableOnly=!comparableOnly;render()});
}
function renderAttempts(){const query=document.getElementById('attempt-filter').value.trim().toLowerCase();const rows=REPORT.attempts.filter(attempt=>!query||[attempt.runId,attempt.result,attempt.command,attempt.error?.code,...attempt.experiments,...attempt.configurations].join(' ').toLowerCase().includes(query)).sort((a,b)=>b.atMs-a.atMs);document.getElementById('attempt-count').textContent=rows.length+' / '+REPORT.attempts.length+' attempts';document.getElementById('attempt-table').innerHTML=rows.map(attempt=>{const failed=!["complete","recorded","running"].includes(attempt.result);const profile=[...attempt.experiments,...attempt.configurations].filter((value,index,array)=>array.indexOf(value)===index).slice(0,3).join(' · ')||'—';return '<tr><td class="mono">'+esc(dateLabel(attempt.atMs))+'</td><td class="'+(failed?'failed':'')+'">'+esc(attempt.result)+'</td><td>'+attempt.measurements+'</td><td>'+esc(profile)+'</td><td>'+esc(attempt.command)+'</td><td><a class="mono" href="../'+encodeURIComponent(attempt.runId)+'/metrics.json">'+attempt.runId.slice(0,8)+'</a></td><td class="'+(failed?'failed':'')+'">'+esc(attempt.error?.code??'—')+'</td></tr>'}).join('')}
function renderCharts(){document.getElementById('charts').innerHTML=charts.map(renderChart).join('');const count=REPORT.measurements.filter(point=>visibleStages.has(point.stage)&&(!comparableOnly||point.comparable)&&pointMatches(point)).length;document.getElementById('chart-filter-count').textContent=count+' / '+REPORT.measurements.length+' measurement passes'}
function render(){renderStageControls();renderCharts();renderAttempts()}
document.getElementById('attempt-total').textContent=REPORT.summary.recordedAttempts.toLocaleString();document.getElementById('pass-total').textContent=REPORT.summary.measurementPasses.toLocaleString();document.getElementById('measured-total').textContent=REPORT.summary.measuredArtifacts.toLocaleString();document.getElementById('metrics-total').textContent=REPORT.summary.metricsFiles.toLocaleString();document.getElementById('failed-total').textContent=REPORT.summary.failedAttempts.toLocaleString();document.getElementById('attempt-filter').addEventListener('input',renderAttempts);document.getElementById('chart-filter').addEventListener('input',event=>{profileQuery=event.target.value.trim().toLowerCase();renderCharts()});renderActivity();render();
</script>
</body>
</html>`;
}

const options = parseArguments(process.argv.slice(2));
const report = loadDataset(options.artifactRoot);
mkdirSync(options.output, { recursive: true });
mkdirSync(join(options.output, "fonts"), { recursive: true });
for (const font of ["atkinson-hyperlegible-next.woff2", "bricolage-grotesque.woff2", "fragment-mono.woff2"])
  copyFileSync(join(FONT_ROOT, font), join(options.output, "fonts", font));
writeFileSync(join(options.output, "history-data.json"), `${JSON.stringify(report, null, 2)}\n`);
writeFileSync(join(options.output, "history-measurements.csv"), historyCsv(report.measurements));
writeFileSync(join(options.output, "index.html"), htmlDocument(report));
console.log(JSON.stringify({ output: options.output, summary: report.summary }, null, 2));
