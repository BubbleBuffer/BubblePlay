import test from "node:test";
import assert from "node:assert/strict";

import { extractHistoryMeasurements } from "../tools/stream-lab-history-data.mjs";

test("history extraction keeps recorded passes and excludes noise summaries", () => {
  const manifest = {
    runId: "history-run",
    startedAt: "2026-08-28T10:00:00.000Z",
    command: "topology-sweep",
    transport: "wss",
    codec: "hevc",
    quality: { width: 1920, height: 1080, fps: 30 },
  };
  const metrics = {
    result: "complete",
    strategy: "wss-topology-frontier",
    results: [{
      id: "v3-15+1",
      configuration: {
        id: "topology-control",
        bitrateMbps: 6,
        wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
      },
      presentedFps: 29.8,
      latencyP95Ms: 153,
      noise: { presentedFps: 0.08, latencyP95Ms: 2 },
      repetitions: [{
        id: "v3-15+1-r1",
        presentedFps: 29.7,
        latencyP95Ms: 155,
        stabilization: [{
          effectiveEncoderBitrateMbps: 3,
          metrics: { presentedFps: 29.5, latencyP95Ms: 161, encodeP95Ms: 10.9 },
        }],
      }],
    }],
  };

  const points = extractHistoryMeasurements({
    manifest,
    metrics,
    runId: manifest.runId,
    endedAtMs: Date.parse("2026-08-28T10:03:00.000Z"),
  });

  assert.equal(points.length, 3);
  assert.deepEqual(points.map(point => point.stage), ["rollup", "repetition", "calibration"]);
  assert.deepEqual(points.map(point => point.presentedFps), [29.8, 29.7, 29.5]);
  assert.ok(points.every(point => point.configurationId === "topology-control"));
  assert.ok(points.every(point => point.topology === "15+1 / 16"));
  assert.ok(points.every(point => point.comparable === true));
  assert.ok(points[0].atMs < points[1].atMs && points[1].atMs < points[2].atMs);
});

test("history extraction treats a direct run metric as a pass despite nested noise", () => {
  const points = extractHistoryMeasurements({
    runId: "direct-run",
    manifest: {
      startedAt: "2026-09-01T12:00:00.000Z",
      command: "baseline",
      transport: "wss",
      codec: "hevc",
      quality: { width: 1920, height: 1080, fps: 30 },
    },
    metrics: {
      result: "complete",
      configuration: { id: "baseline", bitrateMbps: 6 },
      presentedFps: 29.3,
      latencyP95Ms: 226,
      noise: { presentedFps: 0.2, latencyP95Ms: 4 },
    },
    endedAtMs: Date.parse("2026-09-01T12:01:00.000Z"),
  });

  assert.equal(points.length, 1);
  assert.equal(points[0].stage, "run");
  assert.equal(points[0].configurationId, "baseline");
  assert.equal(points[0].latencyP95Ms, 226);
});
