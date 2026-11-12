import assert from "node:assert/strict";
import test from "node:test";
import { decide } from "../ci-changes.mjs";

const jobsFor = files => Object.entries(decide(files)).filter(([, run]) => run).map(([job]) => job).sort();

test("each component change selects only its jobs", () => {
  assert.deepEqual(jobsFor(["relay-rs/core/src/lib.rs"]), ["e2e", "relay"]);
  assert.deepEqual(jobsFor(["worker/public/receiver.js"]), ["browser", "e2e"]);
  assert.deepEqual(jobsFor(["host/src/main.rs"]), ["host", "packaging"]);
  assert.deepEqual(jobsFor(["client-win/src/main.rs"]), ["client", "packaging"]);
  assert.deepEqual(jobsFor(["android/app/build.gradle.kts"]), ["android"]);
  assert.deepEqual(jobsFor(["app/lib/main.dart"]), ["android", "packaging"]);
  assert.deepEqual(jobsFor(["fixtures/wss-contract.json"]), ["android", "browser", "client", "e2e", "host", "relay"]);
  assert.deepEqual(jobsFor(["docs/development.md"]), ["packaging"]);
});

test("CI and tool changes, and runs without a base, select everything", () => {
  const everything = ["android", "browser", "client", "e2e", "host", "packaging", "relay"];
  assert.deepEqual(jobsFor([".github/workflows/checks.yml"]), everything);
  assert.deepEqual(jobsFor(["tools/check.mjs"]), everything);
  assert.deepEqual(jobsFor(null), everything);
  assert.deepEqual(jobsFor([]), []);
});
