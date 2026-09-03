import { appendFileSync, mkdirSync, readFileSync, readdirSync, renameSync, writeFileSync } from "node:fs";
import { dirname, extname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

export const STREAM_LAB_SCHEMA_VERSION = 1;

export const SCENES = Object.freeze([
  { id: "ui-text", seed: 469, kind: "static", description: "English and German UI text at 8–24 px" },
  { id: "thin-lines", seed: 469, kind: "static", description: "Thin rules, icons, controls, and saturated chroma" },
  { id: "gradients", seed: 469, kind: "static", description: "Gradients and flat fields for banding and blocking" },
  { id: "scrolling", seed: 469, kind: "temporal", description: "Deterministic scrolling text and window motion" },
  { id: "motion-60", seed: 469, kind: "temporal", description: "Sixty-frame-per-second motion" },
  { id: "localized-change", seed: 469, kind: "temporal", description: "Static desktop with small localized updates" },
]);

export function percentile(values, fraction) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil(sorted.length * fraction) - 1))];
}

export function median(values) {
  return percentile(values, 0.5);
}

export function medianAbsoluteDeviation(values) {
  const center = median(values);
  return center === null ? null : median(values.map((value) => Math.abs(value - center)));
}

export function characterErrorRate(reference, observed) {
  const expected = normalizeText(reference);
  const actual = normalizeText(observed);
  if (!expected.length) return actual.length ? 1 : 0;
  let previous = Array.from({ length: actual.length + 1 }, (_, index) => index);
  for (let row = 1; row <= expected.length; row++) {
    const current = [row];
    for (let column = 1; column <= actual.length; column++) {
      const substitution = previous[column - 1] + (expected[row - 1] === actual[column - 1] ? 0 : 1);
      current[column] = Math.min(previous[column] + 1, current[column - 1] + 1, substitution);
    }
    previous = current;
  }
  return Math.min(1, previous[actual.length] / expected.length);
}

function normalizeText(value) {
  return String(value ?? "").replace(/\r\n?/g, "\n").normalize("NFC");
}

export function scoreLumaTiles(reference, candidate, width, height, options = {}) {
  const columns = options.columns ?? 8;
  const rows = options.rows ?? 4;
  if (!Number.isInteger(width) || !Number.isInteger(height) || width <= 0 || height <= 0)
    throw new TypeError("width and height must be positive integers");
  if (reference.length !== width * height || candidate.length !== reference.length)
    throw new RangeError("luma buffers must exactly match width × height");
  const tiles = [];
  for (let row = 0; row < rows; row++) {
    const top = Math.floor(row * height / rows);
    const bottom = Math.floor((row + 1) * height / rows);
    for (let column = 0; column < columns; column++) {
      const left = Math.floor(column * width / columns);
      const right = Math.floor((column + 1) * width / columns);
      tiles.push(tileSsim(reference, candidate, width, left, top, right, bottom));
    }
  }
  return {
    columns,
    rows,
    tiles,
    mean: average(tiles),
    p1: percentile(tiles, 0.01),
    p5: percentile(tiles, 0.05),
    worst: Math.min(...tiles),
  };
}

export function temporalUnchangedRegionError(referenceA, referenceB, decodedA, decodedB, width, height, options = {}) {
  const columns = options.columns ?? 8;
  const rows = options.rows ?? 4;
  const unchangedThreshold = options.unchangedThreshold ?? 2;
  const buffers = [referenceA, referenceB, decodedA, decodedB];
  if (buffers.some(buffer => buffer.length !== width * height)) throw new RangeError("temporal luma buffers must exactly match width × height");
  const tiles = [];
  let unchangedPixels = 0;
  let totalError = 0;
  for (let row = 0; row < rows; row++) {
    const top = Math.floor(row * height / rows), bottom = Math.floor((row + 1) * height / rows);
    for (let column = 0; column < columns; column++) {
      const left = Math.floor(column * width / columns), right = Math.floor((column + 1) * width / columns);
      let tilePixels = 0, tileError = 0;
      for (let y = top; y < bottom; y++) for (let x = left; x < right; x++) {
        const index = y * width + x;
        const referenceDelta = referenceB[index] - referenceA[index];
        if (Math.abs(referenceDelta) > unchangedThreshold) continue;
        const error = Math.abs((decodedB[index] - decodedA[index]) - referenceDelta) / 255;
        tilePixels++; unchangedPixels++; tileError += error; totalError += error;
      }
      tiles.push(tilePixels ? tileError / tilePixels : 0);
    }
  }
  return {
    unchangedPixels,
    mean: unchangedPixels ? totalError / unchangedPixels : 0,
    worstTile: Math.max(...tiles),
    p95Tile: percentile(tiles, 0.95),
    tiles,
  };
}

function tileSsim(reference, candidate, stride, left, top, right, bottom) {
  const count = Math.max(1, (right - left) * (bottom - top));
  let sumA = 0;
  let sumB = 0;
  for (let y = top; y < bottom; y++) for (let x = left; x < right; x++) {
    sumA += reference[y * stride + x];
    sumB += candidate[y * stride + x];
  }
  const meanA = sumA / count;
  const meanB = sumB / count;
  let varianceA = 0;
  let varianceB = 0;
  let covariance = 0;
  for (let y = top; y < bottom; y++) for (let x = left; x < right; x++) {
    const a = reference[y * stride + x] - meanA;
    const b = candidate[y * stride + x] - meanB;
    varianceA += a * a;
    varianceB += b * b;
    covariance += a * b;
  }
  const divisor = Math.max(1, count - 1);
  varianceA /= divisor;
  varianceB /= divisor;
  covariance /= divisor;
  const c1 = (0.01 * 255) ** 2;
  const c2 = (0.03 * 255) ** 2;
  return ((2 * meanA * meanB + c1) * (2 * covariance + c2)) /
    ((meanA ** 2 + meanB ** 2 + c1) * (varianceA + varianceB + c2));
}

function average(values) {
  return values.reduce((sum, value) => sum + value, 0) / Math.max(1, values.length);
}

export function buildFfmpegQualityCommand(referencePath, decodedPath, outputPath) {
  const logPath = ffmpegFilterValue(resolve(outputPath));
  const stem = resolve(outputPath).slice(0, -extname(outputPath).length || undefined);
  const ssimPath = ffmpegFilterValue(`${stem}.ssim.log`);
  const psnrPath = ffmpegFilterValue(`${stem}.psnr.log`);
  const blurPath = ffmpegFilterValue(`${stem}.blur.log`);
  const blockPath = ffmpegFilterValue(`${stem}.block.log`);
  return {
    file: "ffmpeg",
    args: [
      "-hide_banner", "-nostdin", "-i", referencePath, "-i", decodedPath,
      "-filter_complex",
      `[0:v]settb=AVTB,setpts=PTS-STARTPTS,split=4[refv][refs][refp][refblock];` +
      `[1:v]settb=AVTB,setpts=PTS-STARTPTS,split=5[distv][dists][distp][blur][distblock];` +
      `[distv][refv]libvmaf=log_fmt=json:log_path='${logPath}'[vmafout];[vmafout]nullsink;` +
      `[dists][refs]ssim=stats_file='${ssimPath}'[ssimout];[ssimout]nullsink;` +
      `[distp][refp]psnr=stats_file='${psnrPath}'[psnrout];[psnrout]nullsink;` +
      `[blur]blurdetect,metadata=mode=print:file='${blurPath}'[blurout];[blurout]nullsink;` +
      `[distblock][refblock]blend=all_mode=difference,format=gray,` +
      `blockdetect,metadata=mode=print:file='${blockPath}'[out]`,
      "-map", "[out]", "-f", "null", "-",
    ],
  };
}

export function buildTesseractCommand(imagePath, outputBasePath) {
  return {
    file: "tesseract",
    args: [imagePath, outputBasePath, "-l", "eng+deu", "--psm", "6"],
  };
}

export function runFfmpegQuality(referencePath, decodedPath, outputPath) {
  mkdirSync(dirname(resolve(outputPath)), { recursive: true });
  const command = buildFfmpegQualityCommand(referencePath, decodedPath, outputPath);
  const result = spawnSync(command.file, command.args, { encoding: "utf8", windowsHide: true, timeout: 120_000 });
  if (result.status !== 0) throw new Error(`FFmpeg quality scoring failed: ${tail(result.stderr)}`);
  const stem = resolve(outputPath).slice(0, -extname(outputPath).length || undefined);
  const vmaf = JSON.parse(readFileSync(resolve(outputPath), "utf8"));
  return {
    vmaf: vmaf?.pooled_metrics?.vmaf?.mean ?? null,
    ssim: averageLogMetric(`${stem}.ssim.log`, /All:([0-9.]+)/g),
    psnr: averageLogMetric(`${stem}.psnr.log`, /psnr_avg:([0-9.]+)/g),
    blur: averageMetadataMetric(`${stem}.blur.log`, /lavfi\.blur=([0-9.eE+-]+)/g),
    block: averageMetadataMetric(`${stem}.block.log`, /lavfi\.block=([0-9.eE+-]+)/g),
    files: {
      vmaf: resolve(outputPath), ssim: `${stem}.ssim.log`, psnr: `${stem}.psnr.log`,
      blur: `${stem}.blur.log`, block: `${stem}.block.log`,
    },
  };
}

export function runTesseractOcr(imagePath, outputBasePath, referenceText) {
  mkdirSync(dirname(resolve(outputBasePath)), { recursive: true });
  const command = buildTesseractCommand(imagePath, outputBasePath);
  const result = spawnSync(command.file, command.args, { encoding: "utf8", windowsHide: true, timeout: 60_000 });
  if (result.status !== 0) throw new Error(`Tesseract OCR failed: ${tail(result.stderr)}`);
  const observed = readFileSync(`${outputBasePath}.txt`, "utf8");
  return { observed, reference: referenceText, cer: characterErrorRate(referenceText, observed) };
}

export function readImageLuma(imagePath) {
  const probe = spawnSync("ffprobe", ["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "json", imagePath], {
    encoding: "utf8", windowsHide: true, timeout: 30_000,
  });
  if (probe.status !== 0) throw new Error(`FFprobe image inspection failed: ${tail(probe.stderr)}`);
  const stream = JSON.parse(probe.stdout)?.streams?.[0];
  const width = Number(stream?.width);
  const height = Number(stream?.height);
  if (!Number.isInteger(width) || !Number.isInteger(height)) throw new Error("image has no readable dimensions");
  const decoded = spawnSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-i", imagePath, "-frames:v", "1", "-pix_fmt", "gray", "-f", "rawvideo", "-"], {
    encoding: null, windowsHide: true, timeout: 30_000, maxBuffer: Math.max(16 * 1024 * 1024, width * height + 1024),
  });
  if (decoded.status !== 0) throw new Error(`FFmpeg luma extraction failed: ${tail(decoded.stderr)}`);
  if (decoded.stdout.length !== width * height) throw new Error(`unexpected luma length ${decoded.stdout.length}, expected ${width * height}`);
  return { width, height, luma: new Uint8Array(decoded.stdout) };
}

function averageLogMetric(path, expression) {
  if (!existsFile(path)) return null;
  const values = [...readFileSync(path, "utf8").matchAll(expression)].map(match => Number(match[1])).filter(Number.isFinite);
  return values.length ? average(values) : null;
}

function averageMetadataMetric(path, expression) {
  return averageLogMetric(path, expression);
}

function existsFile(path) {
  try { return readFileSync(path).length >= 0; } catch { return false; }
}

function tail(value, size = 1_000) {
  return String(value ?? "").slice(-size).trim();
}

function ffmpegFilterValue(path) {
  return path.replaceAll("\\", "/").replace(/^([A-Za-z]):/, "$1\\:").replaceAll("'", "\\'");
}

export function hardGateReasons(candidate, baseline = candidate, noise = {}) {
  const reasons = [];
  const expectedTransport = candidate.expectedTransport ?? candidate.transport;
  const expectedCodec = candidate.expectedCodec ?? candidate.codec;
  const hybridMode = candidate?.configuration?.hybridMode ??
    candidate?.configuration?.encoder?.hybridMode ?? "off";
  if (candidate.transport !== expectedTransport) reasons.push("wrong-route");
  if (candidate.codec !== expectedCodec) reasons.push("wrong-codec");
  if (!finiteAtMost(candidate.firstFrameMs, 5_000)) reasons.push("first-frame-timeout");
  if (!finiteAtLeast(candidate.presentedFps, 27)) reasons.push("low-presented-fps");
  if (["png-quiescent-detail", "hevc-quiescent-tiles"].includes(hybridMode)) {
    const analysisP95Us = Number(candidate?.hostHybridActivity?.analysisP95Us);
    if (!Number.isFinite(analysisP95Us)) reasons.push("quiescent-analysis-missing");
    else if (analysisP95Us > 1_500) reasons.push("quiescent-analysis-overhead");
  } else if (!finiteAtMost(candidate.encodeP95Ms, 10)) reasons.push("encode-latency");
  const transportCeiling = candidate.transport === "webrtc" ? 200 : 500;
  const regressionAllowance = Math.max(15, 2 * (noise.latencyP95Ms ?? 0));
  if (!finiteAtMost(candidate.latencyP95Ms, transportCeiling) ||
      candidate.latencyP95Ms > baseline.latencyP95Ms + regressionAllowance) reasons.push("stream-latency");
  if (!mouseNeutralComparison(candidate, baseline) &&
      (!Number.isFinite(candidate.actionToPhotonP95Ms) ||
       !Number.isFinite(baseline.actionToPhotonP95Ms) ||
       candidate.actionToPhotonP95Ms > baseline.actionToPhotonP95Ms + 20)) reasons.push("action-latency");
  if ((candidate.freezes ?? 0) > 0) reasons.push("freeze");
  if ((candidate.decoderResets ?? 0) > 0) reasons.push("decoder-reset");
  if ((candidate.dependencyLosses ?? 0) > 0) reasons.push("dependency-loss");
  if ((candidate.spatialTransitions ?? 0) > 0) reasons.push("spatial-transition");
  if ((candidate.queueSlopePerMinute ?? 0) > 0) reasons.push("queue-growth");
  if ((candidate.memorySlopeMbPerMinute ?? 0) > 2) reasons.push("memory-growth");
  if (candidate.bandwidthVerified === false) reasons.push("bandwidth-shaping");
  return [...new Set(reasons)];
}

function finiteAtMost(value, limit) {
  return Number.isFinite(value) && value <= limit;
}

function finiteAtLeast(value, limit) {
  return Number.isFinite(value) && value >= limit;
}

function mouseNeutralComparison(candidate, baseline) {
  const candidateSkipped = actionProbeSkipped(candidate);
  if (!candidateSkipped) return false;
  if (actionProbeSkipped(baseline)) return true;
  return !Number.isFinite(candidate?.actionToPhotonP95Ms) &&
    !Number.isFinite(baseline?.actionToPhotonP95Ms);
}

function actionProbeSkipped(metrics) {
  if (metrics?.actionProbe?.result === "skipped") return true;
  const repetitions = Array.isArray(metrics?.repetitions) ? metrics.repetitions : [];
  return repetitions.length > 0 && repetitions.every(repetition =>
    repetition?.actionProbe?.result === "skipped" ||
    (repetition?.quiescentInteractions?.skipped === true &&
      repetition.quiescentInteractions.reason === "input-automation-disabled"));
}

const QUIESCENT_CACHE_MIN_REDUCTION = 0.5;

export function compareQuiescentCacheEfficiency(baseline, candidate) {
  const baselineMode = baseline?.configuration?.hybridMode ??
    baseline?.configuration?.encoder?.hybridMode ?? "off";
  const candidateMode = candidate?.configuration?.hybridMode ??
    candidate?.configuration?.encoder?.hybridMode ?? "off";
  if (baselineMode !== "png-quiescent-detail" || candidateMode !== "png-quiescent-detail")
    return null;

  const expectedPolicies = {
    baseline: "dda-gated",
    candidate: "suppress-reactivate",
  };
  const observedPolicies = {
    baseline: baseline?.configuration?.hybridCacheMode ??
      baseline?.configuration?.encoder?.hybridCacheMode ?? "suppress-reactivate",
    candidate: candidate?.configuration?.hybridCacheMode ??
      candidate?.configuration?.encoder?.hybridCacheMode ?? "suppress-reactivate",
  };
  if (observedPolicies.baseline !== expectedPolicies.baseline ||
      observedPolicies.candidate !== expectedPolicies.candidate) {
    return {
      minimumReduction: QUIESCENT_CACHE_MIN_REDUCTION,
      expectedPolicies,
      observedPolicies,
      reductions: {},
      reasons: ["quiescent-cache-policy-mismatch"],
    };
  }

  const reasons = [];
  if (baseline?.bandwidthVerified !== true || candidate?.bandwidthVerified !== true)
    reasons.push("quiescent-cache-bandwidth-unverified");
  const control = baseline?.hostHybridActivity;
  const suppression = candidate?.hostHybridActivity;
  const controlFrames = Number(control?.frames);
  const candidateFrames = Number(suppression?.frames);
  if (!control || !suppression || !(controlFrames > 0) || !(candidateFrames > 0)) {
    return {
      minimumReduction: QUIESCENT_CACHE_MIN_REDUCTION,
      reductions: {},
      reasons: [...new Set([...reasons, "quiescent-cache-telemetry-missing"])],
    };
  }

  const normalized = (activity, frames) => {
    const pngBytes = Number(activity.pngBytes);
    const churn = Number(activity.upserts) + Number(activity.invalidations);
    const sidecarBytes = Number(activity.sidecarBytes);
    const analysisP95Us = Number(activity.analysisP95Us);
    const pngUsTotal = Number(activity.pngUsTotal);
    return {
      pngBytes: Number.isFinite(pngBytes) ? pngBytes / frames : null,
      churn: Number.isFinite(churn) ? churn / frames : null,
      sidecarBytes: Number.isFinite(sidecarBytes) ? sidecarBytes / frames : null,
      hostWork: Number.isFinite(analysisP95Us) && Number.isFinite(pngUsTotal)
        ? analysisP95Us + pngUsTotal / frames
        : null,
    };
  };
  const controlPerFrame = normalized(control, controlFrames);
  const candidatePerFrame = normalized(suppression, candidateFrames);
  const reductions = {};
  for (const [metric, reason] of [
    ["pngBytes", "quiescent-cache-png-bytes"],
    ["churn", "quiescent-cache-churn"],
    ["sidecarBytes", "quiescent-cache-sidecar-bytes"],
    ["hostWork", "quiescent-cache-host-work"],
  ]) {
    const before = controlPerFrame[metric];
    const after = candidatePerFrame[metric];
    if (!(Number.isFinite(before) && before > 0 && Number.isFinite(after) && after >= 0)) {
      reasons.push("quiescent-cache-control-inactive");
      continue;
    }
    reductions[metric] = 1 - after / before;
    if (reductions[metric] < QUIESCENT_CACHE_MIN_REDUCTION) reasons.push(reason);
  }

  const suppressions = Number(suppression.suppressions);
  const resolved = Number(suppression.reactivations) + Number(suppression.permanentInvalidations);
  if (!(suppressions > 0)) reasons.push("quiescent-cache-suppression-missing");
  if (!(Number.isFinite(resolved) && resolved > 0 && resolved + 4 >= suppressions))
    reasons.push("quiescent-cache-resolution-missing");
  if (!(Number(suppression.pngBytesAvoided) > 0))
    reasons.push("quiescent-cache-avoidance-missing");
  if (Number(suppression.staleReactivationRejects) > 0)
    reasons.push("quiescent-stale-reactivation");

  return {
    minimumReduction: QUIESCENT_CACHE_MIN_REDUCTION,
    controlPerFrame,
    candidatePerFrame,
    reductions,
    reasons: [...new Set(reasons)],
  };
}

export function compareCandidates(baseline, candidate, options = {}) {
  const noise = options.noise ?? {};
  const reasons = hardGateReasons(candidate, baseline, noise);
  const cacheEfficiency = compareQuiescentCacheEfficiency(baseline, candidate);
  if (cacheEfficiency) reasons.push(...cacheEfficiency.reasons);
  const improvements = {
    ocr: baseline.ocrCer - candidate.ocrCer,
    regional: candidate.worstTileSsim - baseline.worstTileSsim,
    vmaf: candidate.vmaf - baseline.vmaf,
  };
  const thresholds = {
    ocr: Math.max(0.01, 2 * (noise.ocrCer ?? 0)),
    regional: Math.max(0.01, 2 * (noise.worstTileSsim ?? 0)),
    vmaf: Math.max(2, 2 * (noise.vmaf ?? 0)),
  };
  const primaryImprovement = ["ocr", "regional", "vmaf"].find((name) => improvements[name] >= thresholds[name]) ?? null;
  const regressions = [];
  if (candidate.ocrCer > baseline.ocrCer + (noise.ocrCer ?? 0)) regressions.push("ocr-regression");
  if (candidate.worstTileSsim < baseline.worstTileSsim - (noise.worstTileSsim ?? 0)) regressions.push("regional-regression");
  if (candidate.vmaf < baseline.vmaf - (noise.vmaf ?? 0)) regressions.push("vmaf-regression");
  if (!primaryImprovement && !cacheEfficiency) reasons.push("no-material-quality-improvement");
  reasons.push(...regressions);
  return {
    schemaVersion: STREAM_LAB_SCHEMA_VERSION,
    baselineId: baseline.id,
    candidateId: candidate.id,
    accepted: reasons.length === 0,
    primaryImprovement,
    improvements,
    thresholds,
    ...(cacheEfficiency ? { cacheEfficiency } : {}),
    reasons: [...new Set(reasons)],
  };
}

export function rankCandidates(candidates) {
  return [...candidates].sort((a, b) =>
    a.ocrCer - b.ocrCer ||
    b.worstTileSsim - a.worstTileSsim ||
    b.vmaf - a.vmaf ||
    a.wireBitrateMbps - b.wireBitrateMbps ||
    a.latencyP95Ms - b.latencyP95Ms ||
    String(a.id).localeCompare(String(b.id)));
}

export function successiveHalving(candidates, keep = 3) {
  const valid = candidates.filter((candidate) => hardGateReasons(candidate, candidate, {}).length === 0);
  return rankCandidates(valid).slice(0, Math.max(0, keep));
}

const REQUIRED_CAMPAIGN_UNITS = Object.freeze([
  "stability:decoder-fault-recovery", "stability:sharp-viewport-2x",
  "stability:adaptive-bandwidth-staircase", "stability:wss-gaming",
  "stability:webrtc-gaming-unshaped", "stability:webrtc-gaming-relay-8m",
  "stability:h264-fallback-smoke", "stability:post-cleanup-idle",
]);
const REQUIRED_ADAPTIVE_DWELLS = Object.freeze([
  "stability:adaptive-bandwidth:8m-1", "stability:adaptive-bandwidth:4m-1",
  "stability:adaptive-bandwidth:2m", "stability:adaptive-bandwidth:4m-2",
  "stability:adaptive-bandwidth:8m-2",
]);

export function evaluateCampaignAcceptance(report, options = {}) {
  const reasons = [];
  const baseline = report?.baseline ?? {};
  const noise = baseline.noise ?? {};
  const stability = Array.isArray(report?.stability) ? report.stability : [];
  const byId = new Map(stability.map(unit => [unit?.unitId, unit]));
  if (report?.result !== "complete") reasons.push("campaign-incomplete");
  for (const unitId of REQUIRED_CAMPAIGN_UNITS) {
    const unit = byId.get(unitId);
    if (!unit) { reasons.push(`${unitId}:missing`); continue; }
    if (unit.result !== "complete") reasons.push(`${unitId}:${unit.result || "incomplete"}`);
    for (const nested of Array.isArray(unit.dwells) ? unit.dwells : []) {
      if (nested?.result !== "complete") reasons.push(`${nested?.unitId || unitId}:` + (nested?.result || "incomplete"));
    }
  }
  const adaptive = byId.get("stability:adaptive-bandwidth-staircase");
  const adaptiveById = new Map((Array.isArray(adaptive?.dwells) ? adaptive.dwells : []).map(unit => [unit?.unitId, unit]));
  for (const unitId of REQUIRED_ADAPTIVE_DWELLS) {
    const unit = adaptiveById.get(unitId);
    if (!unit) reasons.push(`${unitId}:missing`);
    else if (unit.result !== "complete") reasons.push(`${unitId}:${unit.result || "incomplete"}`);
  }
  addMediaGateReasons(reasons, "work-soak", report?.soak, baseline, noise, { transport: "wss", codec: "hevc", minimumFps: 27 });
  addMediaGateReasons(reasons, "stability:wss-gaming", byId.get("stability:wss-gaming"), baseline, noise,
    { transport: "wss", codec: "hevc", minimumFps: 54 });
  addMediaGateReasons(reasons, "stability:webrtc-gaming-unshaped", byId.get("stability:webrtc-gaming-unshaped"), baseline, noise,
    { transport: "webrtc", codec: "hevc", minimumFps: 54 });
  addMediaGateReasons(reasons, "stability:webrtc-gaming-relay-8m", byId.get("stability:webrtc-gaming-relay-8m"), baseline, noise,
    { transport: "webrtc", codec: "hevc", minimumFps: 54 });
  addMediaGateReasons(reasons, "stability:h264-fallback-smoke", byId.get("stability:h264-fallback-smoke"), baseline, noise,
    { transport: "wss", codec: "h264", minimumFps: 27 });
  const idle = byId.get("stability:post-cleanup-idle");
  const idleThresholds = { encoderPct: 1, memoryDeltaMb: 50, ...(options.idleThresholds ?? {}) };
  if (idle) {
    if (idle.seconds < 60 || idle.sampleCount < 2) reasons.push("stability:post-cleanup-idle:insufficient-evidence");
    if (!idle.telemetryAvailability?.process || !idle.telemetryAvailability?.encoder)
      reasons.push("stability:post-cleanup-idle:telemetry-unavailable");
    if (!Number.isFinite(idle.gpuEncoderMeanPct) || idle.gpuEncoderMeanPct > idleThresholds.encoderPct)
      reasons.push("stability:post-cleanup-idle:encoder-busy");
    if (!Number.isFinite(idle.hostMemoryDeltaMb)) reasons.push("stability:post-cleanup-idle:memory-baseline-unavailable");
    else if (idle.hostMemoryDeltaMb > idleThresholds.memoryDeltaMb)
      reasons.push("stability:post-cleanup-idle:memory-growth");
    const cleanup = idle.cleanupEvidence;
    if (!cleanup || !["service", "displays", "qos", "chrome"].every(key => cleanup.evidenceAvailability?.[key] === true))
      reasons.push("stability:post-cleanup-idle:cleanup-evidence-unavailable");
    if (cleanup && cleanup.serviceStopped !== true) reasons.push("stability:post-cleanup-idle:helper-running");
    if (cleanup && (cleanup.displayResourcesClean ?? cleanup.transientVddAbsent) !== true)
      reasons.push("stability:post-cleanup-idle:vdd-lifecycle");
    if (cleanup && cleanup.qosPoliciesAbsent !== true) reasons.push("stability:post-cleanup-idle:qos-policy-present");
    if (cleanup && cleanup.chromeChildrenExited !== true) reasons.push("stability:post-cleanup-idle:chrome-child-running");
  }
  return { accepted: reasons.length === 0, reasons: [...new Set(reasons)] };
}

function addMediaGateReasons(reasons, label, unit, baseline, noise, expected) {
  if (!unit) { reasons.push(`${label}:missing`); return; }
  const candidate = { ...unit, expectedTransport: expected.transport, expectedCodec: expected.codec };
  for (const reason of hardGateReasons(candidate, baseline, noise)) reasons.push(`${label}:${reason}`);
  if (!Number.isFinite(unit.presentedFps) || unit.presentedFps < expected.minimumFps)
    reasons.push(`${label}:low-presented-fps`);
}

export function artifactStore(root, runId) {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$/.test(runId)) throw new TypeError("invalid runId");
  const path = resolve(root, runId);
  const writeJson = (name, value) => writeFileSync(join(path, name), `${JSON.stringify(value, null, 2)}\n`);
  const writeJsonAtomic = (name, value) => {
    mkdirSync(path, { recursive: true });
    const temporary = join(path, `.${name}.${process.pid}.${Date.now()}.tmp`);
    writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`);
    renameSync(temporary, join(path, name));
  };
  return {
    path,
    initialize(manifest = {}) {
      mkdirSync(path, { recursive: true });
      writeJson("manifest.json", {
        schemaVersion: STREAM_LAB_SCHEMA_VERSION,
        runId,
        startedAt: new Date().toISOString(),
        ...manifest,
      });
      writeFileSync(join(path, "telemetry.ndjson"), "");
    },
    updateManifest(patch = {}) {
      const manifest = JSON.parse(readFileSync(join(path, "manifest.json"), "utf8"));
      writeJsonAtomic("manifest.json", { ...manifest, ...patch });
    },
    appendTelemetry(sample) {
      appendFileSync(join(path, "telemetry.ndjson"), `${JSON.stringify(sample)}\n`);
    },
    writeMetrics(metrics) { writeJson("metrics.json", metrics); },
    writeComparison(comparison) { writeJson("comparison.json", comparison); },
    writeFailure(failure) { writeJsonAtomic("failure-diagnostics.json", failure); },
    writeTopologyCheckpoint(state) { writeJsonAtomic("topology-checkpoint.json", state); },
    readTopologyCheckpoint() { return JSON.parse(readFileSync(join(path, "topology-checkpoint.json"), "utf8")); },
    writeCampaignState(state) { writeJsonAtomic("campaign-state.json", state); },
    readCampaignState() { return JSON.parse(readFileSync(join(path, "campaign-state.json"), "utf8")); },
    listTemporaryFiles() { return readdirSync(path).filter(name => name.endsWith(".tmp")); },
  };
}

export function compareCandidateFiles(baseline, candidate, noise = {}) {
  return compareCandidates(baseline, candidate, { noise });
}
