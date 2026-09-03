import { UserActivityPause } from "./stream-lab-activity.mjs";
import { validateCampaignStateV2 } from "./stream-lab-parity.mjs";

export async function runOptimizationLoop(options) {
  const validation = validateCampaignStateV2(options?.state);
  if (!validation.valid) throw new TypeError(`invalid optimization state: ${validation.reasons.join(",")}`);
  const now = options.now ?? (() => new Date().toISOString());
  const tagActivity = options.activityPolicy === "tag";
  let state = structuredClone(options.state);
  const waitForIdle = async () => options.waitUntilIdle(progress => {
    if (!Number.isFinite(progress?.resumeAt)) return;
    const resumeAfter = new Date(progress.resumeAt).toISOString();
    if (state.pauseState?.resumeAfter === resumeAfter) return;
    const updatedAt = now();
    state = {
      ...state,
      status: "paused",
      updatedAt,
      pauseState: {
        paused: true,
        reason: state.pauseState?.reason ?? "physical-user-activity",
        since: state.pauseState?.since ?? updatedAt,
        resumeAfter,
      },
    };
    options.writeState(state);
  });
  if (state.status === "paused" && state.pauseState?.paused === true) {
    if (!tagActivity) await waitForIdle();
    const resumedAt = now();
    state = {
      ...state,
      status: "running",
      updatedAt: resumedAt,
      pauseState: { paused: false, reason: null, since: null, resumeAfter: null },
    };
    options.writeState(state);
  }
  for (;;) {
    try {
      if (!tagActivity) options.assertRunnable();
      return await options.runEpoch({
        state,
        assertRunnable: tagActivity ? (() => {}) : options.assertRunnable,
        activitySnapshot: options.activitySnapshot,
        writeCheckpoint(nextState) {
          state = structuredClone(nextState);
          options.writeState(state);
        },
      });
    } catch (error) {
      await options.cleanup();
      if (!(error instanceof UserActivityPause)) throw error;
      if (tagActivity) throw error;
      const pausedAt = now();
      state = {
        ...state,
        status: "paused",
        updatedAt: pausedAt,
        pauseState: {
          paused: true,
          reason: error.code,
          since: pausedAt,
          resumeAfter: new Date(error.resumeAt).toISOString(),
        },
      };
      options.writeState(state);
      await waitForIdle();
      const resumedAt = now();
      state = {
        ...state,
        status: "running",
        updatedAt: resumedAt,
        pauseState: { paused: false, reason: null, since: null, resumeAfter: null },
      };
      options.writeState(state);
    }
  }
}
