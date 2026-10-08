import test from "node:test";
import assert from "node:assert/strict";

import { UserActivityPause } from "../tools/lab/stream-lab-activity.mjs";
import { runOptimizationLoop } from "../tools/lab/stream-lab-optimize.mjs";
import { createCampaignStateV2 } from "../tools/lab/stream-lab-parity.mjs";

test("optimization checkpoints a pause, cleans resources, waits for idle, and resumes", async () => {
  const states = [];
  const calls = [];
  const initialState = createCampaignStateV2({ runId: "campaign-01", now: "2026-08-24T12:00:00.000Z" });
  let attempts = 0;
  const result = await runOptimizationLoop({
    state: initialState,
    now: () => "2026-08-24T12:00:01.000Z",
    writeState: state => states.push(structuredClone(state)),
    cleanup: async () => calls.push("cleanup"),
    waitUntilIdle: async () => calls.push("wait"),
    runEpoch: async ({ assertRunnable }) => {
      attempts += 1;
      assertRunnable();
      if (attempts === 1) throw new UserActivityPause(1_000, 601_000);
      return { result: "complete", frontier: [{ id: "winner" }] };
    },
    assertRunnable: () => {},
  });

  assert.equal(attempts, 2);
  assert.deepEqual(calls, ["cleanup", "wait"]);
  assert.ok(states.some(state => state.status === "paused" && state.pauseState.reason === "physical-user-activity"));
  assert.equal(states.at(-1).status, "running");
  assert.equal(result.frontier[0].id, "winner");
});

test("non-activity failures are cleaned once and propagated", async () => {
  let cleanups = 0;
  await assert.rejects(runOptimizationLoop({
    state: createCampaignStateV2({ runId: "campaign-02" }),
    writeState() {},
    cleanup: async () => { cleanups += 1; },
    waitUntilIdle: async () => assert.fail("must not wait"),
    assertRunnable() {},
    runEpoch: async () => { throw new Error("encoder failed"); },
  }), /encoder failed/);
  assert.equal(cleanups, 1);
});

test("a persisted paused checkpoint waits and clears pause state before resuming an epoch", async () => {
  const initial = createCampaignStateV2({ runId: "campaign-resume" });
  initial.status = "paused";
  initial.pauseState = {
    paused: true,
    reason: "physical-user-activity",
    since: "2026-08-25T20:00:00.000Z",
    resumeAfter: "2026-08-25T20:10:00.000Z",
  };
  const events = [];
  const states = [];
  const result = await runOptimizationLoop({
    state: initial,
    writeState: state => states.push(structuredClone(state)),
    cleanup: async () => assert.fail("persisted pause has already cleaned resources"),
    waitUntilIdle: async () => events.push("wait"),
    assertRunnable: () => events.push("assert"),
    runEpoch: async ({ state }) => {
      events.push("epoch");
      assert.equal(state.status, "running");
      assert.equal(state.pauseState.paused, false);
      return { result: "complete" };
    },
  });
  assert.equal(result.result, "complete");
  assert.deepEqual(events, ["wait", "assert", "epoch"]);
  assert.equal(states[0].status, "running");
  assert.equal(states[0].pauseState.paused, false);
});

test("later physical activity republishes the moving idle deadline while paused", async () => {
  const initial = createCampaignStateV2({ runId: "campaign-moving-deadline" });
  const states = [];
  let attempts = 0;
  await runOptimizationLoop({
    state: initial,
    now: () => "2026-08-26T00:00:00.000Z",
    writeState: state => states.push(structuredClone(state)),
    cleanup: async () => {},
    waitUntilIdle: async onProgress => {
      onProgress({ lastPhysicalAt: 5_000, resumeAt: 605_000, remainingMs: 600_000 });
    },
    assertRunnable: () => {},
    runEpoch: async () => {
      attempts += 1;
      if (attempts === 1) throw new UserActivityPause(1_000, 601_000);
      return { result: "complete" };
    },
  });
  const moving = states.find(state => state.pauseState.resumeAfter === new Date(605_000).toISOString());
  assert.ok(moving);
  assert.equal(moving.status, "paused");
  assert.equal(moving.pauseState.since, "2026-08-26T00:00:00.000Z");
});

test("tag policy resumes a persisted pause immediately and never waits for idle", async () => {
  const initial = createCampaignStateV2({ runId: "campaign-tag-policy" });
  initial.status = "paused";
  initial.pauseState = { paused: true, reason: "physical-user-activity",
    since: "2026-08-27T18:00:00.000Z", resumeAfter: "2026-08-27T18:10:00.000Z" };
  const states = [];
  const result = await runOptimizationLoop({
    state: initial,
    activityPolicy: "tag",
    activitySnapshot: () => ({ sequence: 9, active: true }),
    writeState: state => states.push(structuredClone(state)),
    cleanup: async () => assert.fail("tag policy must not tear down for activity"),
    waitUntilIdle: async () => assert.fail("tag policy must not wait"),
    assertRunnable: () => assert.fail("tag policy must not assert idleness"),
    runEpoch: async ({ state, assertRunnable, activitySnapshot }) => {
      assert.doesNotThrow(assertRunnable);
      assert.equal(activitySnapshot().sequence, 9);
      assert.equal(state.status, "running");
      return { result: "complete" };
    },
  });
  assert.equal(result.result, "complete");
  assert.equal(states[0].status, "running");
  assert.equal(states[0].pauseState.paused, false);
});
