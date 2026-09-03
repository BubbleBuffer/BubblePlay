const METRIC_FIELDS = [
  "firstFrameMs",
  "presentedFps",
  "encodeP95Ms",
  "encodeMeanMs",
  "latencyP95Ms",
  "latencyP50Ms",
  "actionToPhotonP95Ms",
  "sourceToFirstShardP95Ms",
  "sourceToHostEgressP95Ms",
  "hostEgressToEdgeP95Ms",
  "hostEgressToFirstShardP95Ms",
  "sourceToEdgeP95Ms",
  "edgeToFirstShardP95Ms",
  "sourceToPresentedP95Ms",
  "encodedBitrateMbps",
  "wireBitrateMbps",
  "offeredWireBitrateMbps",
  "deliveredWireBitrateMbps",
  "wireExpansion",
  "frameGapP95Ms",
  "frameGapMaxMs",
  "hostCpuPct",
  "gpuUtilizationMeanPct",
  "gpuEncoderMeanPct",
  "gpuPowerMeanWatts",
  "memorySlopeMbPerMinute",
  "vmafP5",
  "vmafMean",
  "ocrCer",
  "worstTileSsim",
  "edgeFidelity",
  "exactFrameRatio",
  "queueSlopePerMinute",
  "decoderResets",
  "dependencyLosses",
  "freezes",
  "duplicateFrames",
  "droppedFrames",
];

function finite(value) {
  if (value === null || value === undefined || value === "") return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function metricObject(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  return finite(value.presentedFps) !== null && METRIC_FIELDS.some(field => finite(value[field]) !== null);
}

function pathText(path) {
  return path.map(part => typeof part === "number" ? `[${part}]` : part).join(".");
}

function stageFor(path) {
  const keys = path.filter(part => typeof part === "string");
  const last = keys.at(-1);
  if (keys.includes("stabilization") && last === "metrics") return "calibration";
  if (last === "control" || last === "recovered") return "comparison";
  if (keys.includes("repetitions") || keys.includes("runs")) return "repetition";
  if (last === "metrics" && keys.includes("error")) return "failure-snapshot";
  if (keys.includes("results") || keys.includes("baselines") || keys.includes("candidates")) return "rollup";
  return path.length ? "pass" : "run";
}

function topologyLabel(topology) {
  const lanes = finite(topology?.laneCount);
  const data = finite(topology?.dataShards);
  const parity = finite(topology?.parityShards);
  return lanes !== null && data !== null && parity !== null ? `${data}+${parity} / ${lanes}` : null;
}

function mergedConfiguration(parent, value) {
  const own = value?.configuration ?? value?.effectiveConfiguration;
  return own && typeof own === "object" ? { ...(parent ?? {}), ...own } : parent;
}

function comparableProfile({ manifest, transport, codec, configuration }) {
  const requestedBitrate = finite(configuration?.requestedBitrateMbps ?? configuration?.bitrateMbps);
  return transport === "wss"
    && codec === "hevc"
    && finite(manifest?.quality?.width) === 1920
    && finite(manifest?.quality?.height) === 1080
    && finite(manifest?.quality?.fps) === 30
    && requestedBitrate === 6;
}

export function extractHistoryMeasurements({ manifest = {}, metrics, runId, endedAtMs }) {
  if (!metrics || typeof metrics !== "object") return [];
  const measurements = [];
  const rootTransport = String(metrics.transport ?? manifest.transport ?? "");
  const rootCodec = String(metrics.codec ?? manifest.codec ?? "");

  function walk(value, path, context) {
    if (!value || typeof value !== "object") return;
    if (Array.isArray(value)) {
      value.forEach((item, index) => walk(item, [...path, index], context));
      return;
    }

    const configuration = mergedConfiguration(context.configuration, value);
    const topology = value.topology ?? configuration?.wssTopology ?? context.topology ?? null;
    const transport = String(value.transport ?? context.transport ?? rootTransport);
    const codec = String(value.codec ?? context.codec ?? rootCodec);
    const nextContext = { configuration, topology, transport, codec };

    if (metricObject(value)) {
      const normalized = Object.fromEntries(METRIC_FIELDS.map(field => [field, finite(value[field])]));
      const configurationId = String(configuration?.id ?? value.configuration?.id ?? "") || null;
      const experimentId = String(configuration?.experimentId ?? metrics.strategy ?? metrics.objective
        ?? configurationId ?? manifest.command ?? "unclassified");
      measurements.push({
        pointId: `${runId}:${pathText(path) || "root"}`,
        runId,
        path: pathText(path) || "root",
        stage: stageFor(path),
        result: String(value.result ?? metrics.result ?? "recorded"),
        command: String(manifest.command ?? "unknown"),
        strategy: String(metrics.strategy ?? metrics.objective ?? "") || null,
        configurationId,
        experimentId,
        implementationVariant: String(configuration?.implementationVariant ?? "") || null,
        transport,
        codec,
        width: finite(manifest?.quality?.width),
        height: finite(manifest?.quality?.height),
        fpsTarget: finite(manifest?.quality?.fps),
        requestedBitrateMbps: finite(configuration?.requestedBitrateMbps ?? configuration?.bitrateMbps),
        topology: topologyLabel(topology),
        comparable: comparableProfile({ manifest, transport, codec, configuration }),
        ...normalized,
      });
    }

    for (const [key, child] of Object.entries(value)) {
      if (key === "noise") continue;
      if (child && typeof child === "object") walk(child, [...path, key], nextContext);
    }
  }

  walk(metrics, [], { configuration: null, topology: null, transport: rootTransport, codec: rootCodec });

  const manifestStart = Date.parse(manifest.startedAt ?? "");
  const fallbackEnd = finite(endedAtMs) ?? Date.now();
  const startAtMs = Number.isFinite(manifestStart) ? manifestStart : fallbackEnd;
  const endAtMs = Math.max(startAtMs, fallbackEnd);
  const span = Math.max(measurements.length + 1, 1);
  return measurements.map((measurement, index) => ({
    ...measurement,
    atMs: startAtMs + (endAtMs - startAtMs) * (index + 1) / span,
  }));
}
