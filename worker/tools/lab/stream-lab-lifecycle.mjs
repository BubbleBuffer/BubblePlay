// Lease, checkpoint, and process termination ownership. No browser or scoring dependencies.
export async function runCheckpointedUnit(context, unitId, reconcile, measure) {
  await reconcile?.();
  if (Object.hasOwn(context.completedUnits, unitId)) return context.completedUnits[unitId];
  const before = context.activitySnapshot?.();
  const result = attachActivityEvidence(await measure(), before, context.activitySnapshot?.());
  context.completedUnits[unitId] = result;
  await context.onCheckpoint?.(unitId, result);
  return result;
}

export async function runFreshCheckpoint(context, unitId, reconcile, measure) {
  const historical = context.completedUnits[unitId];
  await reconcile?.();
  const before = context.activitySnapshot?.();
  const measured = attachActivityEvidence(await measure(), before, context.activitySnapshot?.());
  const result = historical ? { ...measured, resumeHistory: {
    result: historical.result ?? null,
    seconds: Number.isFinite(historical.seconds) ? historical.seconds : null,
    sampleCount: Number.isFinite(historical.sampleCount) ? historical.sampleCount : null,
  } } : measured;
  context.completedUnits[unitId] = result;
  await context.onCheckpoint?.(unitId, result);
  return result;
}

function attachActivityEvidence(result, before, after) {
  if (!before && !after) return result;
  const startSequence = Number(before?.sequence) || 0;
  const endSequence = Number(after?.sequence) || startSequence;
  const performanceContaminated = before?.active === true || after?.active === true || endSequence !== startSequence;
  return {
    ...result,
    performanceContaminated,
    latencyAccuracy: performanceContaminated ? "possibly-inaccurate" : "verified",
    activityEvidence: {
      policy: "tag",
      startSequence,
      endSequence,
      lastPhysicalAt: after?.lastPhysicalAt ?? before?.lastPhysicalAt ?? null,
      performanceContaminated,
    },
  };
}

export class SerializedLeaseHeartbeat {
  constructor(renew, options = {}) {
    this.renew = renew;
    this.intervalMs = options.intervalMs ?? 240_000;
    this.schedule = options.schedule ?? ((callback, delay) => setTimeout(callback, delay));
    this.cancel = options.cancel ?? (timer => clearTimeout(timer));
    this.timer = null;
    this.inFlight = null;
    this.running = false;
    this.stopping = false;
    this.error = null;
  }
  start() {
    if (this.running) return;
    this.running = true;
    this.scheduleNext();
  }
  scheduleNext() {
    if (!this.running || this.stopping || this.error) return;
    this.timer = this.schedule(() => { this.timer = null; void this.tick(); }, this.intervalMs);
  }
  async tick() {
    if (!this.running || this.stopping || this.inFlight || this.error) return;
    this.inFlight = Promise.resolve().then(this.renew);
    try { await this.inFlight; }
    catch (error) { this.error = error instanceof Error ? error : new Error(String(error)); }
    finally {
      this.inFlight = null;
      if (!this.error) this.scheduleNext();
    }
  }
  assertHealthy() {
    if (this.error) throw new StreamLabUnavailable("candidate-lease-renewal-failed",
      `The local candidate lease renewal failed: ${this.error.message}`, { cause: this.error.message });
  }
  async stop() {
    this.stopping = true;
    this.running = false;
    if (this.timer !== null) this.cancel(this.timer);
    this.timer = null;
    try { await this.inFlight; }
    catch (error) { this.error ??= error instanceof Error ? error : new Error(String(error)); }
    this.assertHealthy();
  }
}

export function installTerminationCleanup(cleanup, options = {}) {
  const target = options.target ?? process;
  const hardExit = options.hardExit ?? (code => process.exit(code));
  const timeoutMs = options.timeoutMs ?? 10_000;
  let handling = false, exited = false, timer = null;
  const exitOnce = code => { if (exited) return; exited = true; hardExit(code); };
  const onSignal = signal => {
    const code = signal === "SIGINT" ? 130 : 143;
    if (handling) { exitOnce(code); return; }
    handling = true;
    timer = setTimeout(() => exitOnce(code), timeoutMs);
    timer.unref?.();
    const finish = () => {
      if (timer) clearTimeout(timer);
      exitOnce(code);
    };
    Promise.resolve().then(() => cleanup(signal)).then(finish, error => {
      console.error("Stream Lab signal cleanup failed:", error);
      finish();
    });
  };
  const onInt = () => onSignal("SIGINT"), onTerm = () => onSignal("SIGTERM");
  target.on("SIGINT", onInt); target.on("SIGTERM", onTerm);
  return () => {
    target.off?.("SIGINT", onInt); target.off?.("SIGTERM", onTerm);
    if (timer) clearTimeout(timer);
  };
}

export function interruptedCampaignState(state, signal, at = new Date().toISOString()) {
  return { ...state, status: "interrupted",
    pauseState: { paused: false, reason: null, since: null, resumeAfter: null },
    error: { code: signal === "SIGTERM" ? "terminated" : "interrupted", message: `Stream Lab received ${signal}.` },
    interruptedAt: at, updatedAt: at };
}

export class StreamLabUnavailable extends Error {
  constructor(code, message, detail = null) {
    super(message);
    this.name = "StreamLabUnavailable";
    this.code = code;
    this.detail = detail;
  }
}

