import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { bundleProblem, pinnedWorkerBuildVersion, pinnedWranglerVersion } from "../deploy-relay.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "../..");

test("a bundle is deployable only when stamped with HEAD and not older than it", () => {
  const stamp = { commit: "abc" };
  assert.equal(bundleProblem(stamp, "abc", 2000, 1000), null);
  assert.match(bundleProblem(null, "abc", 2000, 1000), /BUILD_INFO/);
  assert.match(bundleProblem({ commit: "old" }, "abc", 2000, 1000), /built from old/);
  assert.match(bundleProblem(stamp, "abc", 500, 1000), /older than the HEAD commit/);
});

test("the deploy script finds the versions CI pins", () => {
  assert.match(pinnedWorkerBuildVersion(readFileSync(join(root, "relay-rs/worker/Cargo.toml"), "utf8")), /^\d+\.\d+\.\d+$/);
  assert.match(pinnedWranglerVersion(readFileSync(join(root, ".github/workflows/checks.yml"), "utf8")), /^\d+\.\d+\.\d+$/);
});
