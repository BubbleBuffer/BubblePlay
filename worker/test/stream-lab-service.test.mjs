import test from "node:test";
import assert from "node:assert/strict";

import {
  LAB_SERVICE_NAME,
  SERVICE_STATE,
  ServiceControlError,
  buildHelperRequest,
  demandStartHelperRequest,
  endHelperServiceSession,
  openDemandStartedHelperSession,
  parseServiceQuery,
  queryLabService,
  startLabService,
} from "../tools/stream-lab-runtime.mjs";

const runId = "123e4567-e89b-42d3-a456-426614174000";

test("schema-v2 helper apply normalizes the legacy dirty QP-map candidate flag", () => {
  const request = buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: null,
    profile: {
      schemaVersion: 2, implementationVariant: "candidate", experimentId: "dirty-qp-map",
      transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
      width: 1920, height: 1080, fps: 30, bitrateBps: 6_000_000, gopSeconds: 2,
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
      encoder: {
        preset: "p4", tuning: "low-latency", multipass: "quarter",
        aqMode: "spatial", aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0,
        weightedPrediction: false, dirtyQpMap: true, main10: false,
      },
    },
  });
  assert.equal(request.profile.encoder.dirtyQpMap, true);
  assert.equal(request.profile.encoder.roiMode, "damage");
  assert.equal(request.profile.encoder.resourceMode, "per-frame");
  assert.equal(request.profile.encoder.asyncDepth, 0);
  assert.equal(request.profile.encoder.main10, false);
  assert.equal(request.profile.hybridMode, "off");
});

test("schema-v2 helper apply preserves one bounded pipeline experiment axis", () => {
  const request = buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: null,
    profile: {
      schemaVersion: 2, implementationVariant: "candidate", experimentId: "nvenc-persistent",
      transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
      width: 1920, height: 1080, fps: 30, bitrateBps: 6_000_000, gopSeconds: 2,
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
      hybridMode: "off",
      encoder: {
        preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
        aqMode: "spatial", aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0,
        weightedPrediction: false, dirtyQpMap: false, main10: false,
        roiMode: "off", resourceMode: "persistent", asyncDepth: 0,
      },
    },
  });
  assert.equal(request.profile.encoder.resourceMode, "persistent");
  assert.throws(() => buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: null,
    profile: {
      ...request.profile,
      hybridMode: "png-static-roi",
      encoder: { ...request.profile.encoder, roiMode: "damage" },
    },
  }), /experiment|axis|controls/i);
});

test("schema-v2 helper admits the bounded CQ23 external-budget VBR axis", () => {
  const request = buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: 6,
    profile: {
      schemaVersion: 2, implementationVariant: "candidate", experimentId: "external-budget-vbr-cq23",
      transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
      width: 1920, height: 1080, fps: 30, bitrateBps: 6_000_000,
      requestedBitrateBps: 6_000_000, totalMediaCapBps: 6_000_000, gopSeconds: 2,
      wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
      hybridMode: "off", hybridCacheMode: "suppress-reactivate",
      encoder: {
        preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
        aqMode: "spatial", aqStrength: 8, vbvFrames: 4, lookaheadDepth: 0,
        weightedPrediction: false, dirtyQpMap: false, main10: false,
        roiMode: "off", resourceMode: "per-frame", asyncDepth: 0,
        rateControlMode: "external-budget-vbr", targetQuality: 23, peakBitrateMultiplier: 4,
      },
    },
  });
  assert.equal(request.profile.encoder.rateControlMode, "external-budget-vbr");
  assert.equal(request.profile.encoder.targetQuality, 23);
});

test("schema-v2 helper apply accepts the quiescence-gated hybrid experiment", () => {
  const base = {
    schemaVersion: 2, implementationVariant: "candidate",
    experimentId: "hybrid-png-quiescent-detail",
    transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
    width: 1920, height: 1080, fps: 30, bitrateBps: 6_000_000, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
    hybridMode: "png-quiescent-detail",
    hybridCacheMode: "suppress-reactivate",
    encoder: {
      preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
      aqMode: "spatial", aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0,
      weightedPrediction: false, dirtyQpMap: false, main10: false,
      roiMode: "off", resourceMode: "per-frame", asyncDepth: 0,
    },
  };
  const request = buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: null, profile: base,
  });
  assert.equal(request.profile.hybridMode, "png-quiescent-detail");
  assert.equal(request.profile.hybridCacheMode, "suppress-reactivate");

  const ddaRequest = buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: null,
    profile: { ...base, experimentId: "hybrid-png-quiescent-detail-dda-gated", hybridCacheMode: "dda-gated" },
  });
  assert.equal(ddaRequest.profile.hybridCacheMode, "dda-gated");
});

test("helper admission binds a calibrated pipeline target to the requested six-megabit cohort and active cap", () => {
  const profile = {
    schemaVersion: 2, implementationVariant: "candidate", experimentId: "hybrid-cache-calibrated",
    transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
    width: 1920, height: 1080, fps: 30, bitrateBps: 5_400_000,
    requestedBitrateBps: 6_000_000, totalMediaCapBps: 6_000_000, gopSeconds: 2,
    wssTopology: { laneCount: 16, dataShards: 15, parityShards: 1 },
    hybridMode: "png-quiescent-detail", hybridCacheMode: "suppress-reactivate",
    encoder: {
      preset: "p3", tuning: "ultra-low-latency", multipass: "disabled",
      aqMode: "spatial", aqStrength: 8, vbvFrames: 1, lookaheadDepth: 0,
      weightedPrediction: false, dirtyQpMap: false, main10: false,
      roiMode: "off", resourceMode: "per-frame", asyncDepth: 0,
    },
  };

  const request = buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: 6, profile,
  });
  assert.equal(request.profile.bitrateBps, 5_400_000);
  assert.equal(request.profile.requestedBitrateBps, 6_000_000);
  assert.equal(request.profile.totalMediaCapBps, 6_000_000);
  assert.throws(() => buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: 4, profile,
  }), /experiment|control|cap/i);
  assert.throws(() => buildHelperRequest("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: 6,
    profile: { ...profile, requestedBitrateBps: 5_400_000 },
  }), /experiment|control|6 Mbps/i);
});

test("service query parsing depends on numeric SCM state, not localized words", () => {
  const result = parseServiceQuery({
    status: 0,
    stdout: "SERVICE_NAME: BubblePlayLabHelper\r\n        TYP : 10  WIN32_OWN_PROCESS\r\n        ZUSTAND : 4  WIRD_AUSGEFÜHRT\r\n",
    stderr: "",
  });
  assert.deepEqual(result, { installed: true, queryable: true, state: SERVICE_STATE.RUNNING, stateName: "running" });
  assert.equal(LAB_SERVICE_NAME, "BubblePlayLabHelper");
});

test("SCM boundaries address only the fixed demand-start service", () => {
  const calls = [];
  const state = queryLabService({ scRunner: args => {
    calls.push(args);
    return { status: 0, stdout: "ETAT : 1 ARRETE", stderr: "" };
  } });
  startLabService({ scRunner: args => { calls.push(args); return { status: 0, stdout: "", stderr: "" }; } });
  assert.equal(state.state, SERVICE_STATE.STOPPED);
  assert.deepEqual(calls, [["query", LAB_SERVICE_NAME], ["start", LAB_SERVICE_NAME]]);
});

test("missing and denied service states have stable actionable codes", () => {
  assert.equal(parseServiceQuery({ status: 1060, stdout: "beliebiger lokalisierter Text", stderr: "" }).installed, false);
  assert.throws(() => parseServiceQuery({ status: 5, stdout: "beliebiger lokalisierter Text", stderr: "" }),
    error => error.code === "service-query-denied" && /administrator/i.test(error.message));
  assert.throws(() => startLabService({ scRunner: () => ({ status: 5, stdout: "", stderr: "" }) }),
    error => error.code === "service-start-denied" && /permission/i.test(error.message));
});

test("an already-open helper pipe is the fast path and does not start SCM", async () => {
  let starts = 0;
  const reply = await demandStartHelperRequest("begin", { runId, leaseSeconds: 600, vdd: true }, {
    pipeRequest: async () => ({ ok: true, event: "begun" }),
    startService: () => { starts++; },
    queryService: () => { throw new Error("SCM should not be queried"); },
  });
  assert.equal(reply.event, "begun");
  assert.equal(starts, 0);
});

test("a stopped installed service starts through SCM and serves the unchanged request", async () => {
  let starts = 0;
  let pipeAttempts = 0;
  const reply = await demandStartHelperRequest("begin", { runId, leaseSeconds: 600, vdd: true }, {
    pipeRequest: async (command, input) => {
      pipeAttempts++;
      if (pipeAttempts === 1) throw pipeMissing();
      assert.equal(command, "begin");
      assert.deepEqual(input, { runId, leaseSeconds: 600, vdd: true });
      return { ok: true, event: "begun" };
    },
    queryService: () => ({ installed: true, queryable: true, state: SERVICE_STATE.STOPPED, stateName: "stopped" }),
    startService: () => { starts++; },
    sleep: async () => {}, startTimeoutMs: 5,
  });
  assert.equal(reply.event, "begun");
  assert.equal(starts, 1);
});

test("the first admitted begin request keeps the pipe open for slow VDD setup", async () => {
  const observedTimeouts = [];
  let attempts = 0;
  const reply = await demandStartHelperRequest("begin", { runId, leaseSeconds: 600, vdd: true }, {
    pipeRequest: async (_command, _input, options) => {
      observedTimeouts.push(options.timeoutMs);
      if (attempts++ === 0) throw pipeMissing();
      return { ok: true, event: "begun" };
    },
    queryService: () => ({ installed: true, queryable: true, state: SERVICE_STATE.STOPPED, stateName: "stopped" }),
    startService: () => {}, sleep: async () => {}, startTimeoutMs: 20_000,
  });
  assert.equal(reply.event, "begun");
  assert.ok(observedTimeouts[1] >= 5_000, `admitted begin timeout was ${observedTimeouts[1]} ms`);
});

test("SCM start denial is actionable and never falls back to UAC", async () => {
  await assert.rejects(demandStartHelperRequest("begin", { runId, leaseSeconds: 600, vdd: true }, {
    pipeRequest: async () => { throw pipeMissing(); },
    queryService: () => ({ installed: true, queryable: true, state: SERVICE_STATE.STOPPED, stateName: "stopped" }),
    startService: () => { throw new ServiceControlError("service-start-denied", "raw denial"); },
  }), error => error.code === "service-start-denied" && /administrator|service permission/i.test(error.message));
});

test("one stale stopping instance is allowed to stop and restart once", async () => {
  let starts = 0;
  let pipeAttempts = 0;
  const states = [SERVICE_STATE.RUNNING, SERVICE_STATE.STOP_PENDING, SERVICE_STATE.STOPPED];
  const reply = await demandStartHelperRequest("begin", { runId, leaseSeconds: 600, vdd: true }, {
    pipeRequest: async () => {
      pipeAttempts++;
      if (pipeAttempts < 3) throw pipeMissing();
      return { ok: true, event: "begun" };
    },
    queryService: () => {
      const state = states.shift() ?? SERVICE_STATE.STOPPED;
      return { installed: true, queryable: true, state, stateName: "fixture" };
    },
    startService: () => { starts++; },
    sleep: async () => {}, startTimeoutMs: 0, stopTimeoutMs: 5,
  });
  assert.equal(reply.event, "begun");
  assert.equal(starts, 1);
});

test("end waits for the demand-start service to report stopped", async () => {
  const states = [SERVICE_STATE.STOP_PENDING, SERVICE_STATE.STOPPED];
  const result = await endHelperServiceSession(runId, {
    pipeRequest: async (command, input) => {
      assert.equal(command, "end"); assert.deepEqual(input, { runId });
      return { ok: true, event: "ended" };
    },
    queryService: () => {
      const state = states.shift() ?? SERVICE_STATE.STOPPED;
      return { installed: true, queryable: true, state, stateName: "fixture" };
    },
    sleep: async () => {}, stopTimeoutMs: 10,
  });
  assert.equal(result.stopped, true);
});

test("a campaign keeps one authenticated pipe from begin through apply and end", async () => {
  const commands = [];
  let opens = 0;
  let starts = 0;
  let closes = 0;
  const pipe = {
    async request(command, input) {
      commands.push({ command, input });
      return { ok: true, event: `${command}ed` };
    },
    close() { closes++; },
  };
  const states = [SERVICE_STATE.STOPPED, SERVICE_STATE.STOP_PENDING, SERVICE_STATE.STOPPED];
  const session = await openDemandStartedHelperSession({ runId, leaseSeconds: 600, vdd: true }, {
    openPipeSession: async () => {
      opens++;
      if (opens === 1) throw pipeMissing();
      return pipe;
    },
    queryService: () => {
      const state = states.shift() ?? SERVICE_STATE.STOPPED;
      return { installed: true, queryable: true, state, stateName: "fixture" };
    },
    startService: () => { starts++; },
    sleep: async () => {}, startTimeoutMs: 5, stopTimeoutMs: 5,
  });
  await session.request("apply", {
    runId, leaseSeconds: 600, bandwidthMbps: 4,
    profile: {
      schemaVersion: 1, transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
      width: 1920, height: 1080, fps: 30, bitrateBps: 4_000_000,
      encoder: { preset: "p4", tuning: "low-latency", multipass: "quarter", aqStrength: 8, temporalAq: true, vbvFrames: 2 },
    },
  });
  await session.end(runId);
  assert.equal(starts, 1);
  assert.equal(opens, 2);
  assert.equal(closes, 1);
  assert.deepEqual(commands.map(item => item.command), ["begin", "apply", "end"]);
});

test("a live helper session renews its lease without reapplying the profile", async () => {
  const commands = [];
  const pipe = { async request(command, input, options) { commands.push({ command, input, options }); return { ok: true, event: command }; }, close() {} };
  const states = [SERVICE_STATE.STOP_PENDING, SERVICE_STATE.STOPPED];
  const session = await openDemandStartedHelperSession({ runId, leaseSeconds: 600, vdd: true }, {
    openPipeSession: async () => pipe,
    queryService: () => { const state = states.shift() ?? SERVICE_STATE.STOPPED; return { installed: true, queryable: true, state, stateName: "fixture" }; },
    sleep: async () => {}, stopTimeoutMs: 5,
  });
  await session.renew(runId, 600);
  await session.shape(runId, null, 600);
  await session.end(runId);
  assert.deepEqual(commands.map(item => item.command), ["begin", "renew", "shape", "end"]);
  assert.deepEqual(commands[1].input, { runId, leaseSeconds: 600 });
  assert.deepEqual(commands[2].input, { runId, leaseSeconds: 600, bandwidthMbps: null });
  assert.equal(commands[1].options.timeoutMs, 30_000);
  assert.equal(commands[2].options.timeoutMs, 30_000);
  assert.equal(commands[3].options.timeoutMs, 30_000);
});

test("helper session serializes heartbeat renewal with an overlapping shape transition", async () => {
  const commands = [];
  let active = false;
  const pipe = {
    async request(command) {
      if (active) throw new Error("stream-lab helper permits one in-flight command");
      active = true;
      commands.push(command);
      await new Promise(resolve => setTimeout(resolve, 5));
      active = false;
      return { ok: true, event: command === "shape" ? "shaped" : command };
    },
    close() {},
  };
  const session = await openDemandStartedHelperSession({ runId, leaseSeconds: 600, vdd: false }, {
    openPipeSession: async () => pipe,
    queryService: () => ({ installed: true, queryable: true, state: SERVICE_STATE.STOPPED, stateName: "fixture" }),
    sleep: async () => {}, stopTimeoutMs: 5,
  });
  await Promise.all([session.renew(runId, 600), session.shape(runId, 4, 600)]);
  await session.end(runId);
  assert.deepEqual(commands, ["begin", "renew", "shape", "end"]);
});

function pipeMissing() {
  const error = new Error("named pipe absent");
  error.code = "ENOENT";
  return error;
}
