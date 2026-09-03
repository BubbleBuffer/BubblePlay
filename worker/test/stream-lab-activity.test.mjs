import test from "node:test";
import assert from "node:assert/strict";

import {
  PhysicalActivityTracker,
  UserActivityPause,
} from "../tools/stream-lab-activity.mjs";

test("physical activity pauses a campaign until the complete idle window", () => {
  const tracker = new PhysicalActivityTracker({ idleMs: 600_000 });
  tracker.acceptLine('{"event":"ready","pid":42}', 1_000);
  assert.doesNotThrow(() => tracker.assertRunnable(1_000));

  tracker.acceptLine('{"event":"physical-input","sequence":1,"atMs":1500}', 1_500);
  assert.throws(() => tracker.assertRunnable(601_499), UserActivityPause);
  assert.doesNotThrow(() => tracker.assertRunnable(601_500));
});

test("malformed and injected-event-shaped output cannot fabricate physical activity", () => {
  const tracker = new PhysicalActivityTracker({ idleMs: 10_000 });
  tracker.acceptLine("not-json", 1_000);
  tracker.acceptLine('{"event":"injected-input","sequence":9,"atMs":1000}', 1_000);
  assert.equal(tracker.lastPhysicalAt, null);
  assert.doesNotThrow(() => tracker.assertRunnable(1_001));
});

test("activity snapshots expose contamination evidence without throwing", () => {
  const tracker = new PhysicalActivityTracker({ idleMs: 600_000 });
  tracker.acceptLine('{"event":"ready","pid":42}', 1_000);
  assert.deepEqual(tracker.snapshot(1_100), {
    sequence: 0, lastPhysicalAt: null, observedAt: 1_100, active: false,
  });
  tracker.acceptLine('{"event":"physical-input","sequence":7,"atMs":1200}', 1_200);
  assert.deepEqual(tracker.snapshot(1_300), {
    sequence: 7, lastPhysicalAt: 1_200, observedAt: 1_300, active: true,
  });
});
