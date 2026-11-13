import test from "node:test";
import assert from "node:assert/strict";
import { createOnceReporter } from "../public/report.js";

test("a failing path leaves one log line with its cause, however often it fails", () => {
  const lines = [];
  const report = createOnceReporter((level, event, fields) => lines.push({ level, event, fields }));
  assert.equal(report("input-channel-send-failed", new Error("closing")), true);
  assert.equal(report("input-channel-send-failed", new Error("closing")), false);
  assert.equal(report("other", "text", { channel: "motion" }), true);
  assert.deepEqual(lines, [
    { level: "warn", event: "input-channel-send-failed", fields: { error: "closing" } },
    { level: "warn", event: "other", fields: { error: "text", channel: "motion" } },
  ]);
});
