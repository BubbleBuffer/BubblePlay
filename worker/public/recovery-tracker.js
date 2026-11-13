// Bookkeeping for recovery after a lost signaling connection. everAuthenticated: this page had a
// session before, so a silent PC is retried. lossMs: clock time of the loss, kept until the first
// frame is shown again. budgetStartMs: start of the retry budget, restarted by each authentication.
// attempts: signaling connections opened since the loss. The decisions themselves are pure
// functions in recovery.js.
import {
  connectTimeoutMs, createSuspendTracker, recycledAuthTimeoutMs, shouldRetryAuthTimeout, splitRecovery,
} from "./recovery.js";

// env: now() in ms, log(level, event, detail), suspend (optional tracker, for tests).
export function createRecoveryTracker(env) {
  const suspend = env.suspend ?? createSuspendTracker();
  let everAuthenticated = false;
  let lossMs = null;
  let suspendedAtLossMs = 0;
  let budgetStartMs = null;
  let attempts = 0;

  return {
    get recovering() { return lossMs !== null; },
    get attempts() { return attempts; },
    sinceLossMs() { return lossMs === null ? null : Math.round(env.now() - lossMs); },
    freeze() { suspend.freeze(env.now()); },
    resume() { suspend.resume(env.now()); },
    tick() { suspend.tick(env.now()); },

    noteLoss() {
      if (!everAuthenticated) return;
      const now = env.now();
      if (lossMs === null) { lossMs = now; attempts = 0; suspendedAtLossMs = suspend.totalMs(now); }
      if (budgetStartMs === null) budgetStartMs = now;
    },
    noteAuthenticated() {
      everAuthenticated = true;
      budgetStartMs = null;
    },
    // A signaling connection was opened: counts as an attempt while recovering.
    noteConnectionOpened() { if (lossMs !== null) attempts++; },
    // Wait for the PC's auth-host: 10 s for a first connection, shorter and backing off for a recycled session.
    authTimeoutMs() {
      return everAuthenticated && lossMs !== null ? recycledAuthTimeoutMs(attempts - 1) : undefined;
    },
    // A reconnect of an established session abandons a socket that has not opened in time; null when not recovering.
    connectTimeoutMs() { return connectTimeoutMs({ recovering: lossMs !== null, attempt: attempts - 1 }); },
    // A silent PC on a recycled session retries on a new signaling connection while the budget lasts.
    shouldRetryAuthTimeout(reason) {
      return shouldRetryAuthTimeout({ reason, established: everAuthenticated, budgetStartMs, nowMs: env.now() });
    },
    noteRecovered() {
      if (lossMs === null) return;
      const now = env.now();
      // Frozen or suspended time is reported apart: activeMs is what the recovery target measures.
      const { recoveryMs, suspendedMs, activeMs } = splitRecovery(now - lossMs, suspend.totalMs(now) - suspendedAtLossMs);
      const count = attempts;
      lossMs = budgetStartMs = null;
      attempts = 0;
      env.log("info", `recovered after ${activeMs} ms, ${count} attempts`, { recoveryMs, activeMs, suspendedMs, attempts: count });
    },
  };
}
