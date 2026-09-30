import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { connect } from "node:net";
import { spawnSync } from "node:child_process";

export const STREAM_LAB_PIPE = "\\\\.\\pipe\\BubblePlay.StreamLab.v1";
export const LAB_SERVICE_NAME = "BubblePlayLabHelper";
export const SERVICE_STATE = Object.freeze({
  STOPPED: 1, START_PENDING: 2, STOP_PENDING: 3, RUNNING: 4,
  CONTINUE_PENDING: 5, PAUSE_PENDING: 6, PAUSED: 7,
});
const MAX_HELPER_MESSAGE_BYTES = 64 * 1024;

const STATE_NAMES = Object.freeze({
  1: "stopped", 2: "start-pending", 3: "stop-pending", 4: "running",
  5: "continue-pending", 6: "pause-pending", 7: "paused",
});

export class ServiceControlError extends Error {
  constructor(code, message, detail = null) {
    super(message);
    this.name = "ServiceControlError";
    this.code = code;
    this.detail = detail;
  }
}

function runSc(arguments_, options = {}) {
  const systemRoot = options.env?.SystemRoot || process.env.SystemRoot || "C:\\Windows";
  return spawnSync(join(systemRoot, "System32", "sc.exe"), arguments_, {
    encoding: "utf8", windowsHide: true, timeout: options.timeoutMs ?? 10_000,
  });
}

export function parseServiceQuery(result) {
  if (result?.error?.code === "ENOENT")
    throw new ServiceControlError("sc-unavailable", "Windows Service Control Manager client sc.exe is unavailable.");
  const status = Number(result?.status);
  if (status !== 0) {
    if (status === 1060) return { installed: false, queryable: true, state: null, stateName: "missing" };
    if (status === 5) throw new ServiceControlError("service-query-denied",
      `Windows denied querying ${LAB_SERVICE_NAME}; repair the installation as an administrator.`, { status });
    throw new ServiceControlError("service-query-failed", `Unable to query ${LAB_SERVICE_NAME} through SCM.`, { status });
  }
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  const state = output.split(/\r?\n/).map(line => line.match(/:\s*([1-7])(?:\s+|$)/)?.[1]).find(Boolean);
  if (!state) throw new ServiceControlError("service-query-unparseable",
    `SCM returned no numeric service state for ${LAB_SERVICE_NAME}.`);
  const numeric = Number(state);
  return { installed: true, queryable: true, state: numeric, stateName: STATE_NAMES[numeric] };
}

export function queryLabService(options = {}) {
  return parseServiceQuery((options.scRunner ?? runSc)(["query", LAB_SERVICE_NAME], options));
}

export function inspectLabService(options = {}) {
  try { return queryLabService(options); }
  catch (error) {
    return { installed: null, queryable: false, state: null, stateName: "unknown",
      error: { code: error.code || "service-query-failed", message: error.message } };
  }
}

export function startLabService(options = {}) {
  const result = (options.scRunner ?? runSc)(["start", LAB_SERVICE_NAME], options);
  if (result?.error?.code === "ENOENT")
    throw new ServiceControlError("sc-unavailable", "Windows Service Control Manager client sc.exe is unavailable.");
  const status = Number(result?.status);
  if (status === 0 || status === 1056) return;
  if (status === 5) throw new ServiceControlError("service-start-denied",
    `Windows denied starting ${LAB_SERVICE_NAME}; run BubblePlay Repair as administrator to restore the service permission.`, { status });
  if (status === 1060) throw new ServiceControlError("service-missing",
    `${LAB_SERVICE_NAME} is not installed; run the BubblePlay service installer as administrator.`, { status });
  throw new ServiceControlError("service-start-failed", `SCM could not start ${LAB_SERVICE_NAME}.`, { status });
}

export async function demandStartHelperRequest(command, input, options = {}) {
  const pipeRequest = options.pipeRequest ?? sendHelperRequest;
  const requestTimeoutMs = Math.min(20_000, Math.max(5_000, options.requestTimeoutMs ?? 10_000));
  try { return await pipeRequest(command, input, { timeoutMs: requestTimeoutMs }); }
  catch (error) { if (!pipeUnavailable(error)) throw error; }

  const queryService = options.queryService ?? (() => queryLabService(options));
  const startService = options.startService ?? (() => startLabService(options));
  const sleep = options.sleep ?? (milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds)));
  const startTimeoutMs = Math.min(20_000, Math.max(0, options.startTimeoutMs ?? 20_000));
  const stopTimeoutMs = Math.min(20_000, Math.max(0, options.stopTimeoutMs ?? 20_000));
  let service = requireInstalledService(queryService());

  for (let attempt = 0; attempt < 2; attempt++) {
    if (service.state === SERVICE_STATE.STOP_PENDING)
      service = await waitForStopped({ queryService, sleep, timeoutMs: stopTimeoutMs });
    if (service.state === SERVICE_STATE.STOPPED) {
      try { startService(); }
      catch (error) {
        if (error?.code === "service-start-denied") throw new ServiceControlError("service-start-denied",
          `Windows denied starting ${LAB_SERVICE_NAME}; run BubblePlay Repair as administrator to restore the service permission.`, error.detail);
        throw error;
      }
    }

    const reply = await pollPipe(command, input, {
      pipeRequest, sleep, timeoutMs: startTimeoutMs, requestTimeoutMs,
    });
    if (reply) return reply;
    const afterPoll = requireInstalledService(queryService());
    if (attempt === 0 && [SERVICE_STATE.STOP_PENDING, SERVICE_STATE.STOPPED].includes(afterPoll.state)) {
      service = afterPoll.state === SERVICE_STATE.STOPPED ? afterPoll :
        await waitForStopped({ queryService, sleep, timeoutMs: stopTimeoutMs });
      continue;
    }
    throw new ServiceControlError("service-pipe-timeout",
      `${LAB_SERVICE_NAME} did not open ${STREAM_LAB_PIPE} within ${startTimeoutMs} ms.`, afterPoll);
  }
  throw new ServiceControlError("service-pipe-timeout", `${LAB_SERVICE_NAME} did not become ready after one stale-instance retry.`);
}

export async function endHelperServiceSession(runId, options = {}) {
  const pipeRequest = options.pipeRequest ?? sendHelperRequest;
  const queryService = options.queryService ?? (() => queryLabService(options));
  const sleep = options.sleep ?? (milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds)));
  try { await pipeRequest("end", { runId }, { timeoutMs: 5_000 }); }
  catch (error) {
    if (!pipeUnavailable(error)) throw error;
    const state = requireInstalledService(queryService());
    if (state.state === SERVICE_STATE.STOPPED) return { stopped: true, alreadyStopped: true };
    throw new ServiceControlError("service-pipe-unavailable",
      `${LAB_SERVICE_NAME} is ${state.stateName} but its control pipe is unavailable.`, state);
  }
  const stopped = await waitForStopped({ queryService, sleep,
    timeoutMs: Math.min(10_000, Math.max(0, options.stopTimeoutMs ?? 10_000)) });
  return { stopped: stopped.state === SERVICE_STATE.STOPPED, alreadyStopped: false };
}

export async function openDemandStartedHelperSession(beginInput, options = {}) {
  const openPipeSession = options.openPipeSession ?? (() => openHelperPipeSession(options));
  const queryService = options.queryService ?? (() => queryLabService(options));
  const startService = options.startService ?? (() => startLabService(options));
  const sleep = options.sleep ?? (milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds)));
  const startTimeoutMs = Math.min(20_000, Math.max(0, options.startTimeoutMs ?? 20_000));
  const stopTimeoutMs = Math.min(20_000, Math.max(0, options.stopTimeoutMs ?? 20_000));
  let service;
  let pipeSession;

  try { pipeSession = await openPipeSession(); }
  catch (error) {
    if (!pipeUnavailable(error)) throw error;
    service = requireInstalledService(queryService());
  }

  for (let attempt = 0; !pipeSession && attempt < 2; attempt++) {
    if (service.state === SERVICE_STATE.STOP_PENDING)
      service = await waitForStopped({ queryService, sleep, timeoutMs: stopTimeoutMs });
    if (service.state === SERVICE_STATE.STOPPED) startService();
    pipeSession = await pollPipeSession({ openPipeSession, sleep, timeoutMs: startTimeoutMs });
    if (pipeSession) break;
    const afterPoll = requireInstalledService(queryService());
    if (attempt === 0 && [SERVICE_STATE.STOP_PENDING, SERVICE_STATE.STOPPED].includes(afterPoll.state)) {
      service = afterPoll.state === SERVICE_STATE.STOPPED ? afterPoll :
        await waitForStopped({ queryService, sleep, timeoutMs: stopTimeoutMs });
      continue;
    }
    throw new ServiceControlError("service-pipe-timeout",
      `${LAB_SERVICE_NAME} did not open ${STREAM_LAB_PIPE} within ${startTimeoutMs} ms.`, afterPoll);
  }
  if (!pipeSession) throw new ServiceControlError("service-pipe-timeout",
    `${LAB_SERVICE_NAME} did not become ready after one stale-instance retry.`);

  try {
    await pipeSession.request("begin", beginInput, { timeoutMs: options.requestTimeoutMs ?? 20_000 });
  } catch (error) {
    pipeSession.close();
    throw error;
  }
  let ended = false;
  const privilegedMutationTimeoutMs = options.privilegedMutationTimeoutMs ?? 30_000;
  let commandTail = Promise.resolve();
  const serialize = operation => {
    const command = commandTail.then(operation, operation);
    commandTail = command.catch(() => {});
    return command;
  };
  return {
    request(command, input, requestOptions) {
      if (ended) throw new ServiceControlError("service-session-ended", "The Stream Lab helper session has ended.");
      return serialize(() => pipeSession.request(command, input, requestOptions));
    },
    renew(runId, leaseSeconds = 600) {
      if (ended) throw new ServiceControlError("service-session-ended", "The Stream Lab helper session has ended.");
      return serialize(() => pipeSession.request("renew", { runId, leaseSeconds }, { timeoutMs: privilegedMutationTimeoutMs }));
    },
    shape(runId, bandwidthMbps, leaseSeconds = 600) {
      if (ended) throw new ServiceControlError("service-session-ended", "The Stream Lab helper session has ended.");
      return serialize(() => pipeSession.request("shape", { runId, leaseSeconds, bandwidthMbps }, { timeoutMs: privilegedMutationTimeoutMs }));
    },
    async end(runId) {
      if (ended) return { stopped: true, alreadyStopped: true };
      ended = true;
      await serialize(async () => {
        try { await pipeSession.request("end", { runId }, { timeoutMs: privilegedMutationTimeoutMs }); }
        finally { pipeSession.close(); }
      });
      const stopped = await waitForStopped({ queryService, sleep, timeoutMs: Math.min(10_000, stopTimeoutMs) });
      return { stopped: stopped.state === SERVICE_STATE.STOPPED, alreadyStopped: false };
    },
    close() {
      ended = true;
      pipeSession.close();
    },
  };
}

export function openHelperPipeSession(options = {}) {
  return new Promise((resolveSession, reject) => {
    const socket = connect(options.pipe ?? STREAM_LAB_PIPE);
    let settled = false;
    socket.once("connect", () => {
      settled = true;
      resolveSession(new HelperPipeSession(socket));
    });
    socket.once("error", error => {
      if (settled) return;
      settled = true;
      socket.destroy();
      reject(error);
    });
  });
}

class HelperPipeSession {
  constructor(socket) {
    this.socket = socket;
    this.response = "";
    this.pending = null;
    this.closed = false;
    socket.setEncoding("utf8");
    socket.on("data", chunk => this.onData(chunk));
    socket.on("error", error => this.fail(error));
    socket.on("close", () => this.fail(new Error("stream-lab helper pipe closed")));
  }

  request(command, input, options = {}) {
    if (this.closed) return Promise.reject(new Error("stream-lab helper pipe is closed"));
    if (this.pending) return Promise.reject(new Error("stream-lab helper permits one in-flight command"));
    const request = `${JSON.stringify(buildHelperRequest(command, input))}\n`;
    if (Buffer.byteLength(request) > MAX_HELPER_MESSAGE_BYTES)
      return Promise.reject(new RangeError("helper request exceeds 64 KiB"));
    return new Promise((resolveRequest, reject) => {
      const timeout = setTimeout(() => {
        this.fail(new Error("stream-lab helper timed out"));
        this.close();
      }, options.timeoutMs ?? 5_000);
      this.pending = { resolveRequest, reject, timeout };
      this.socket.write(request, error => {
        if (error) this.fail(error);
      });
    });
  }

  onData(chunk) {
    this.response += chunk;
    if (Buffer.byteLength(this.response) > MAX_HELPER_MESSAGE_BYTES) {
      this.fail(new Error("helper response exceeds 64 KiB"));
      return this.close();
    }
    const newline = this.response.indexOf("\n");
    if (newline < 0 || !this.pending) return;
    const line = this.response.slice(0, newline);
    this.response = this.response.slice(newline + 1);
    const pending = this.pending;
    this.pending = null;
    clearTimeout(pending.timeout);
    try {
      const reply = JSON.parse(line);
      if (reply.ok !== true) pending.reject(new Error(reply.error || "stream-lab helper rejected request"));
      else pending.resolveRequest(reply);
    } catch (error) { pending.reject(error); }
  }

  fail(error) {
    if (!this.pending) return;
    const pending = this.pending;
    this.pending = null;
    clearTimeout(pending.timeout);
    pending.reject(error);
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    this.fail(new Error("stream-lab helper pipe closed"));
    this.socket.destroy();
  }
}

function requireInstalledService(service) {
  if (service?.installed === false) throw new ServiceControlError("service-missing",
    `${LAB_SERVICE_NAME} is not installed; run the BubblePlay service installer as administrator.`);
  if (!service?.queryable) throw new ServiceControlError(service?.error?.code || "service-query-failed",
    service?.error?.message || `Unable to query ${LAB_SERVICE_NAME}.`);
  return service;
}

async function pollPipe(command, input, options) {
  const deadline = Date.now() + options.timeoutMs;
  do {
    try { return await options.pipeRequest(command, input, { timeoutMs: options.requestTimeoutMs }); }
    catch (error) { if (!pipeUnavailable(error)) throw error; }
    if (Date.now() >= deadline) break;
    await options.sleep(Math.min(250, Math.max(0, deadline - Date.now())));
  } while (Date.now() <= deadline);
  return null;
}

async function pollPipeSession(options) {
  const deadline = Date.now() + options.timeoutMs;
  do {
    try { return await options.openPipeSession(); }
    catch (error) { if (!pipeUnavailable(error)) throw error; }
    if (Date.now() >= deadline) break;
    await options.sleep(Math.min(250, Math.max(0, deadline - Date.now())));
  } while (Date.now() <= deadline);
  return null;
}

async function waitForStopped(options) {
  const deadline = Date.now() + options.timeoutMs;
  do {
    const service = requireInstalledService(options.queryService());
    if (service.state === SERVICE_STATE.STOPPED) return service;
    if (Date.now() >= deadline) break;
    await options.sleep(Math.min(250, Math.max(0, deadline - Date.now())));
  } while (Date.now() <= deadline);
  throw new ServiceControlError("service-stop-timeout",
    `${LAB_SERVICE_NAME} did not report stopped within ${options.timeoutMs} ms.`);
}

function pipeUnavailable(error) {
  return ["ENOENT", "ECONNREFUSED", "EPIPE", "ERROR_PIPE_BUSY"].includes(error?.code);
}

export function validateRunId(runId) {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(runId ?? ""))
    throw new TypeError("runId must be a canonical UUID");
}

function validateLease(value) {
  if (!Number.isInteger(value) || value < 1 || value > 600)
    throw new RangeError("leaseSeconds must be an integer from 1 to 600");
}

function validateBenchmarkProfile(profile, bandwidthMbps = null) {
  if (!profile || ![1, 2].includes(profile.schemaVersion)) throw new TypeError("profile schemaVersion must be 1 or 2");
  if (!["wss", "webrtc"].includes(profile.transport)) throw new RangeError("profile transport must be wss or webrtc");
  if (profile.codec !== "hevc") throw new RangeError("benchmark helper profiles must use hevc");
  if (!["work", "game"].includes(profile.streamMode)) throw new RangeError("benchmark profile streamMode must be work or game");
  if (profile.adaptive !== false) throw new TypeError("benchmark profiles must disable adaptive control");
  if (!Number.isInteger(profile.width) || profile.width < 320 || profile.width > 3840) throw new RangeError("invalid profile width");
  if (!Number.isInteger(profile.height) || profile.height < 180 || profile.height > 2160) throw new RangeError("invalid profile height");
  const maxFps = profile.streamMode === "game" ? 60 : 30;
  if (!Number.isInteger(profile.fps) || profile.fps < 1 || profile.fps > maxFps) throw new RangeError("invalid profile fps");
  const gopSeconds = profile.gopSeconds ?? 2;
  if (![1, 2, 4].includes(gopSeconds)) throw new RangeError("profile gopSeconds must be 1, 2, or 4");
  const bitrateBounds = profile.schemaVersion === 2 ? [500_000, 12_000_000] : [500_000, 40_000_000];
  if (!Number.isInteger(profile.bitrateBps) || profile.bitrateBps < bitrateBounds[0] || profile.bitrateBps > bitrateBounds[1])
    throw new RangeError("profile bitrateBps is outside the schema bounds");
  const requestedBitrateBps = profile.requestedBitrateBps ?? profile.bitrateBps;
  const totalMediaCapBps = profile.totalMediaCapBps ?? profile.bitrateBps;
  if (!Number.isInteger(requestedBitrateBps) || requestedBitrateBps < bitrateBounds[0] || requestedBitrateBps > bitrateBounds[1])
    throw new RangeError("profile requestedBitrateBps is outside the schema bounds");
  if (!Number.isInteger(totalMediaCapBps) || totalMediaCapBps < bitrateBounds[0] || totalMediaCapBps > bitrateBounds[1])
    throw new RangeError("profile totalMediaCapBps is outside the schema bounds");
  const encoder = profile.encoder;
  const presets = profile.schemaVersion === 2 ? ["p1", "p2", "p3", "p4", "p5", "p6", "p7"] : ["p1", "p3", "p4", "p5"];
  if (!encoder || !presets.includes(encoder.preset)) throw new RangeError("invalid profile preset");
  if (!["ultra-low-latency", "low-latency"].includes(encoder.tuning)) throw new RangeError("invalid profile tuning");
  if (!(profile.schemaVersion === 2 ? ["disabled", "quarter", "full"] : ["disabled", "quarter"]).includes(encoder.multipass))
    throw new RangeError("invalid profile multipass");
  if (!Number.isInteger(encoder.aqStrength) || encoder.aqStrength < 1 || encoder.aqStrength > 15) throw new RangeError("invalid AQ strength");
  if (!Number.isInteger(encoder.vbvFrames) || encoder.vbvFrames < 1 || encoder.vbvFrames > 4) throw new RangeError("invalid VBV depth");
  if (profile.schemaVersion === 2) {
    if (!["production", "candidate"].includes(profile.implementationVariant)) throw new RangeError("invalid implementationVariant");
    if (typeof profile.experimentId !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(profile.experimentId))
      throw new RangeError("invalid experimentId");
    if (!["none", "spatial", "temporal"].includes(encoder.aqMode)) throw new RangeError("invalid AQ mode");
    const lookaheadLimit = profile.streamMode === "game" ? 1 : 2;
    if (!Number.isInteger(encoder.lookaheadDepth) || encoder.lookaheadDepth < 0 || encoder.lookaheadDepth > lookaheadLimit)
      throw new RangeError("invalid lookahead depth");
    if (typeof encoder.weightedPrediction !== "boolean") throw new TypeError("weightedPrediction must be boolean");
    if (typeof encoder.dirtyQpMap !== "boolean") throw new TypeError("dirtyQpMap must be boolean");
    if (typeof encoder.main10 !== "boolean") throw new TypeError("main10 must be boolean");
    const roiMode = encoder.roiMode ?? (encoder.dirtyQpMap ? "damage" : "off");
    const resourceMode = encoder.resourceMode ?? "per-frame";
    const asyncDepth = encoder.asyncDepth ?? 0;
    const rateControlMode = encoder.rateControlMode ?? "cbr";
    const hybridMode = profile.hybridMode ?? "off";
    const hybridCacheMode = profile.hybridCacheMode ?? "suppress-reactivate";
    if (!["off", "damage", "text-edge", "feedback", "feedback-headroom"].includes(roiMode)) throw new RangeError("invalid roiMode");
    if (!["per-frame", "persistent"].includes(resourceMode)) throw new RangeError("invalid resourceMode");
    if (![0, 1].includes(asyncDepth)) throw new RangeError("invalid asyncDepth");
    if (!["cbr", "external-budget-vbr"].includes(rateControlMode)) throw new RangeError("invalid rateControlMode");
    if (rateControlMode === "cbr" && (encoder.targetQuality !== undefined || encoder.peakBitrateMultiplier !== undefined))
      throw new RangeError("CBR cannot carry VBR quality controls");
    if (rateControlMode === "external-budget-vbr" &&
        (encoder.targetQuality !== 23 || encoder.peakBitrateMultiplier !== 4 || encoder.vbvFrames !== 4))
      throw new RangeError("external-budget VBR is bounded to CQ23, a 4x ceiling, and four-frame VBV");
    if (!["off", "png-static-roi", "png-quiescent-detail", "hevc-quiescent-tiles"].includes(
      hybridMode))
      throw new RangeError("invalid hybridMode");
    if (!["dda-gated", "suppress-reactivate"].includes(hybridCacheMode))
      throw new RangeError("invalid hybridCacheMode");
    if (hybridCacheMode === "dda-gated" && hybridMode !== "png-quiescent-detail")
      throw new RangeError("hybridCacheMode dda-gated requires png-quiescent-detail");
    if (encoder.dirtyQpMap && roiMode !== "damage")
      throw new RangeError("legacy dirtyQpMap can only normalize to roiMode damage");
    const activeAxes = Number(roiMode !== "off") + Number(hybridMode !== "off") +
      Number(resourceMode !== "per-frame" || asyncDepth !== 0) + Number(rateControlMode !== "cbr");
    if (activeAxes > 0) {
      if (activeAxes !== 1 || profile.implementationVariant !== "candidate" || profile.transport !== "wss" ||
          profile.streamMode !== "work" || profile.width !== 1920 || profile.height !== 1080 ||
          profile.fps !== 30 || requestedBitrateBps !== 6_000_000 || totalMediaCapBps !== 6_000_000 ||
          profile.bitrateBps > requestedBitrateBps || profile.bitrateBps > totalMediaCapBps ||
          (profile.totalMediaCapBps !== undefined && bandwidthMbps !== 6) ||
          encoder.main10 || (asyncDepth === 1 && resourceMode !== "persistent"))
        throw new RangeError("pipeline experiment controls require one candidate axis on the 1080p30 6 Mbps WSS control");
    }
    return {
      schemaVersion: 2, implementationVariant: profile.implementationVariant, experimentId: profile.experimentId,
      transport: profile.transport, codec: profile.codec, streamMode: profile.streamMode, adaptive: false,
      width: profile.width, height: profile.height, fps: profile.fps, bitrateBps: profile.bitrateBps, gopSeconds,
      ...(profile.requestedBitrateBps !== undefined ? { requestedBitrateBps } : {}),
      ...(profile.totalMediaCapBps !== undefined ? { totalMediaCapBps } : {}),
      hybridMode,
      hybridCacheMode,
      encoder: { preset: encoder.preset, tuning: encoder.tuning, multipass: encoder.multipass,
        aqMode: encoder.aqMode, aqStrength: encoder.aqStrength, vbvFrames: encoder.vbvFrames,
        lookaheadDepth: encoder.lookaheadDepth, weightedPrediction: encoder.weightedPrediction,
        dirtyQpMap: encoder.dirtyQpMap, main10: encoder.main10, roiMode, resourceMode, asyncDepth,
        ...(rateControlMode !== "cbr" ? {
          rateControlMode, targetQuality: encoder.targetQuality,
          peakBitrateMultiplier: encoder.peakBitrateMultiplier,
        } : {}) },
    };
  }
  if (typeof encoder.temporalAq !== "boolean") throw new TypeError("temporalAq must be boolean");
  return {
    schemaVersion: 1, transport: profile.transport, codec: profile.codec, streamMode: profile.streamMode, adaptive: false,
    width: profile.width, height: profile.height, fps: profile.fps, bitrateBps: profile.bitrateBps, gopSeconds,
    encoder: {
      preset: encoder.preset, tuning: encoder.tuning, multipass: encoder.multipass,
      aqStrength: encoder.aqStrength, temporalAq: encoder.temporalAq, vbvFrames: encoder.vbvFrames,
    },
  };
}

export function buildHelperRequest(command, input) {
  validateRunId(input?.runId);
  if (command === "clear" || command === "end") return { command, runId: input.runId };
  validateLease(input?.leaseSeconds);
  if (command === "renew") return { command, runId: input.runId, leaseSeconds: input.leaseSeconds };
  if (command === "shape") {
    if (input.bandwidthMbps !== null && ![2, 4, 6, 8, 12].includes(input.bandwidthMbps))
      throw new RangeError("bandwidthMbps must be null, 2, 4, 6, 8, or 12");
    return { command, runId: input.runId, leaseSeconds: input.leaseSeconds, bandwidthMbps: input.bandwidthMbps };
  }
  if (command === "begin") {
    if (typeof input.vdd !== "boolean") throw new TypeError("vdd must be boolean");
    return { command, runId: input.runId, leaseSeconds: input.leaseSeconds, vdd: input.vdd };
  }
  if (command !== "apply") throw new RangeError(`unknown helper command: ${command}`);
  if (input.bandwidthMbps !== null && ![2, 4, 6, 8, 12].includes(input.bandwidthMbps))
    throw new RangeError("bandwidthMbps must be null, 2, 4, 6, 8, or 12");
  return {
    command, runId: input.runId, leaseSeconds: input.leaseSeconds,
    bandwidthMbps: input.bandwidthMbps, profile: validateBenchmarkProfile(input.profile, input.bandwidthMbps),
  };
}

export function sendHelperRequest(command, input, options = {}) {
  const request = `${JSON.stringify(buildHelperRequest(command, input))}\n`;
  if (Buffer.byteLength(request) > MAX_HELPER_MESSAGE_BYTES) throw new RangeError("helper request exceeds 64 KiB");
  return new Promise((resolveRequest, reject) => {
    const socket = connect(options.pipe ?? STREAM_LAB_PIPE);
    const timeout = setTimeout(() => socket.destroy(new Error("stream-lab helper timed out")), options.timeoutMs ?? 5_000);
    let response = "";
    let settled = false;
    const finish = (callback, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      socket.destroy();
      callback(value);
    };
    socket.setEncoding("utf8");
    socket.on("connect", () => socket.write(request));
    socket.on("data", chunk => {
      response += chunk;
      if (Buffer.byteLength(response) > MAX_HELPER_MESSAGE_BYTES)
        return finish(reject, new Error("helper response exceeds 64 KiB"));
      const newline = response.indexOf("\n");
      if (newline < 0) return;
      try {
        const reply = JSON.parse(response.slice(0, newline));
        if (reply.ok !== true) finish(reject, new Error(reply.error || "stream-lab helper rejected request"));
        else finish(resolveRequest, reply);
      } catch (error) { finish(reject, error); }
    });
    socket.on("error", error => finish(reject, error));
    socket.on("close", () => {
      if (!settled) finish(reject, new Error("stream-lab helper closed without a reply"));
    });
  });
}

export function persistentReceiverProfile(env = process.env) {
  const localAppData = env.LOCALAPPDATA || join(homedir(), "AppData", "Local");
  return join(localAppData, "BubblePlay", "StreamLab", "Chrome");
}

export function findChrome(env = process.env) {
  return [
    env.BUBBLEPLAY_CHROME,
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
  ].filter(Boolean).find(existsSync) ?? null;
}

export function buildEncoderRound(champion, dimension) {
  const values = {
    presetTuning: [
      ["p1", "ultra-low-latency"], ["p1", "low-latency"],
      ["p3", "ultra-low-latency"], ["p3", "low-latency"],
      ["p4", "ultra-low-latency"], ["p4", "low-latency"],
      ["p5", "ultra-low-latency"], ["p5", "low-latency"],
    ],
    multipass: ["disabled", "quarter"],
    vbvFrames: [1, 2, 4],
    aqStrength: [6, 8, 10],
    temporalAq: [false, true],
    gopSeconds: [1, 2, 4],
    bitrateMbps: [2, 4, 6, 8],
  };
  if (!(dimension in values)) throw new RangeError(`unknown encoder dimension: ${dimension}`);
  if (dimension === "presetTuning")
    return values.presetTuning.map(([preset, tuning]) => candidate({ ...champion, preset, tuning }, dimension));
  return values[dimension].map(value => candidate({ ...champion, [dimension]: value }, dimension));
}

function candidate(configuration, dimension) {
  return {
    id: `${dimension}-${configuration.preset}-${configuration.tuning}-${configuration.multipass}-vbv${configuration.vbvFrames}-gop${configuration.gopSeconds ?? 2}-aq${configuration.aqStrength}-taq${Number(configuration.temporalAq)}-${configuration.bitrateMbps ?? 6}m`,
    dimension,
    ...configuration,
    configuration,
  };
}
