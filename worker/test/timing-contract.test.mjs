import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  CONNECT_TIMEOUT_MAX_MS, CONNECT_TIMEOUT_MS, MEDIA_ANSWER_LIMIT_MS, RECOVERY_BUDGET_MS,
  RECYCLED_AUTH_TIMEOUT_MAX_MS, RECYCLED_AUTH_TIMEOUT_MS, SIGNALING_ANSWER_LIMIT_MS, SUSPEND_GAP_MS,
  connectTimeoutMs, recycledAuthTimeoutMs,
} from "../public/recovery.js";
import { AUTH_TIMEOUT_MS } from "../public/dk-session.js";
import { V4_BACKOFF_MAX_MS, V4_BACKOFF_MIN_MS, V4_HEARTBEAT_MS, nextBackoffMsV4 } from "../public/media-v4.js";

// fixtures/session-timing-contract.json, "browser" section: the viewer's timers and retry schedules.
const contract = JSON.parse(readFileSync(new URL("../../fixtures/session-timing-contract.json", import.meta.url), "utf8"));
const { values } = contract.browser;

/** The delays a schedule function gives for attempts 0..n-1. */
const delays = (n, next) => Array.from({ length: n }, (_, attempt) => next(attempt));

test("browser timers match the timing contract", () => {
  assert.equal(V4_HEARTBEAT_MS, values.heartbeatMs);
  assert.equal(SIGNALING_ANSWER_LIMIT_MS, values.signalingAnswerLimitMs);
  assert.equal(MEDIA_ANSWER_LIMIT_MS, values.mediaAnswerLimitMs);
  assert.equal(SUSPEND_GAP_MS, values.suspendGapMs);
  assert.equal(RECOVERY_BUDGET_MS, values.recoveryBudgetMs);
  assert.equal(AUTH_TIMEOUT_MS, values.authHostTimeoutMs);
});

test("browser media backoff follows the contract schedule", () => {
  const backoff = values.mediaBackoff;
  assert.equal(V4_BACKOFF_MIN_MS, backoff.minMs);
  assert.equal(V4_BACKOFF_MAX_MS, backoff.maxMs);
  const expected = contract.schedules[backoff.schedule];
  let previous = 0;
  const seen = expected.map(() => (previous = nextBackoffMsV4(previous)));
  assert.deepEqual(seen, expected);
});

test("browser recycled-session waits follow the contract schedules", () => {
  const auth = values.recycledAuthHost;
  assert.equal(RECYCLED_AUTH_TIMEOUT_MS, auth.firstMs);
  assert.equal(RECYCLED_AUTH_TIMEOUT_MAX_MS, auth.maxMs);
  const authSchedule = contract.schedules[auth.schedule];
  assert.deepEqual(delays(authSchedule.length, recycledAuthTimeoutMs), authSchedule);

  const connect = values.connectTimeout;
  assert.equal(CONNECT_TIMEOUT_MS, connect.firstMs);
  assert.equal(CONNECT_TIMEOUT_MAX_MS, connect.maxMs);
  const connectSchedule = contract.schedules[connect.schedule];
  assert.deepEqual(delays(connectSchedule.length, (attempt) => connectTimeoutMs({ recovering: true, attempt })), connectSchedule);
});
