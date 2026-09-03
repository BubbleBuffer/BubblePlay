import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

export class UserActivityPause extends Error {
  constructor(lastPhysicalAt, resumeAt) {
    super("Stream Lab paused because physical keyboard or mouse activity was detected.");
    this.name = "UserActivityPause";
    this.code = "physical-user-activity";
    this.lastPhysicalAt = lastPhysicalAt;
    this.resumeAt = resumeAt;
  }
}

export class PhysicalActivityTracker {
  constructor(options = {}) {
    this.idleMs = options.idleMs ?? 600_000;
    this.lastPhysicalAt = null;
    this.lastSequence = 0;
    this.ready = false;
  }

  acceptLine(line, observedAt = Date.now()) {
    let message;
    try { message = JSON.parse(String(line)); } catch { return false; }
    if (message?.event === "ready" && Number.isSafeInteger(message.pid) && message.pid > 0) {
      this.ready = true;
      return true;
    }
    if (message?.event !== "physical-input" || !Number.isSafeInteger(message.sequence) ||
        message.sequence <= this.lastSequence) return false;
    this.lastSequence = message.sequence;
    this.lastPhysicalAt = observedAt;
    return true;
  }

  assertRunnable(now = Date.now()) {
    if (this.lastPhysicalAt === null || now - this.lastPhysicalAt >= this.idleMs) return;
    throw new UserActivityPause(this.lastPhysicalAt, this.lastPhysicalAt + this.idleMs);
  }

  remainingIdleMs(now = Date.now()) {
    return this.lastPhysicalAt === null ? 0 : Math.max(0, this.lastPhysicalAt + this.idleMs - now);
  }

  snapshot(now = Date.now()) {
    return {
      sequence: this.lastSequence,
      lastPhysicalAt: this.lastPhysicalAt,
      observedAt: now,
      active: this.remainingIdleMs(now) > 0,
    };
  }
}

export function defaultActivityGuardPath() {
  return resolve(import.meta.dirname, "..", "..", "host", "target-candidate", "release",
    "bubbleplay_lab_activity_guard.exe");
}

export async function launchPhysicalActivityGuard(options = {}) {
  const executable = options.executable ?? defaultActivityGuardPath();
  if (!existsSync(executable)) {
    const error = new Error(`Stream Lab physical-input guard is missing: ${executable}`);
    error.code = "activity-guard-missing";
    throw error;
  }
  const tracker = new PhysicalActivityTracker({ idleMs: options.idleMs });
  const child = spawn(executable, [], { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  const lines = createInterface({ input: child.stdout });
  lines.on("line", line => tracker.acceptLine(line));
  let stderr = "";
  child.stderr.on("data", chunk => { stderr = `${stderr}${chunk}`.slice(-2_000); });
  const ready = await waitFor(() => tracker.ready || child.exitCode !== null, options.readyTimeoutMs ?? 5_000);
  if (!ready || !tracker.ready) {
    child.kill();
    const error = new Error(`Stream Lab physical-input guard did not become ready.${stderr ? ` ${stderr.trim()}` : ""}`);
    error.code = "activity-guard-not-ready";
    throw error;
  }
  return {
    tracker,
    process: child,
    assertRunnable(now) { tracker.assertRunnable(now); },
    snapshot(now) { return tracker.snapshot(now); },
    async waitUntilIdle(onProgress, signal) {
      while (tracker.remainingIdleMs() > 0) {
        if (signal?.aborted) throw signal.reason ?? new Error("activity wait aborted");
        const remainingMs = tracker.remainingIdleMs();
        onProgress?.({ lastPhysicalAt: tracker.lastPhysicalAt,
          resumeAt: tracker.lastPhysicalAt + tracker.idleMs, remainingMs });
        await delay(Math.min(1_000, remainingMs));
      }
    },
    stop() {
      lines.close();
      if (child.exitCode === null) child.kill();
    },
  };
}

async function waitFor(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await delay(25);
  }
  return predicate();
}

function delay(ms) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, Math.max(1, ms)));
}
