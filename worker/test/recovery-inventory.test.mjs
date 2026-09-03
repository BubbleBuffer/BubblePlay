import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  classifyWorkspacePath,
  fingerprintFile,
  redactCommandLine,
  summarizeWorkspaceEntries,
} from "../tools/recovery-inventory.mjs";

test("recovery inventory separates reviewable source from generated and runtime evidence", () => {
  assert.equal(classifyWorkspacePath("host/src/input.rs"), "source");
  assert.equal(classifyWorkspacePath("worker/test/input.test.mjs"), "test");
  assert.equal(classifyWorkspacePath("host/scripts/update-service.ps1"), "deployment");
  assert.equal(classifyWorkspacePath("worker/wrangler.toml"), "configuration");
  assert.equal(classifyWorkspacePath("target-stage1/debug/cache.bin"), "generated");
  assert.equal(classifyWorkspacePath("host/target-privileged/release/bubbleplay-host.exe"), "generated");
  assert.equal(classifyWorkspacePath("artifacts/public-field-sessions/run/host.stderr.log"), "evidence");
  assert.equal(classifyWorkspacePath("host-run-current.err.txt"), "evidence");
});

test("recovery inventory never records bearer, token, or password values from command lines", () => {
  const original = "bubbleplay-host --token swordfish --password=hunter2 " +
    "--authorization 'Bearer secret-bearer' wss://name:pass@example.invalid/ws?token=query-secret";
  const redacted = redactCommandLine(original);

  for (const secret of ["swordfish", "hunter2", "secret-bearer", "name:pass", "query-secret"])
    assert.equal(redacted.includes(secret), false, `leaked ${secret}`);
  assert.match(redacted, /--token \[REDACTED\]/);
  assert.match(redacted, /--password=\[REDACTED\]/);
});

test("small files receive a full SHA-256 while oversized evidence uses a bounded fingerprint", async t => {
  const directory = await mkdtemp(join(tmpdir(), "bubbleplay-recovery-inventory-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const small = join(directory, "small.txt");
  const large = join(directory, "large.log");
  await writeFile(small, "bubbleplay");
  await writeFile(large, Buffer.from("abcdefghijklmnop"));

  const smallResult = await fingerprintFile(small, { fullHashLimitBytes: 32, sampleBytes: 4 });
  assert.equal(smallResult.hashMode, "full");
  assert.equal(smallResult.sha256, createHash("sha256").update("bubbleplay").digest("hex"));

  const largeResult = await fingerprintFile(large, { fullHashLimitBytes: 8, sampleBytes: 4 });
  assert.equal(largeResult.hashMode, "head-tail-size");
  assert.equal(largeResult.bytes, 16);
  assert.equal(largeResult.sampledBytes, 8);
  assert.match(largeResult.sha256, /^[0-9a-f]{64}$/);
});

test("workspace summary reports counts and bytes by preservation class", () => {
  const result = summarizeWorkspaceEntries([
    { path: "host/src/input.rs", bytes: 10, hashMode: "full" },
    { path: "worker/test/input.test.mjs", bytes: 20, hashMode: "full" },
    { path: "target-stage1/debug/cache.bin", bytes: 30, hashMode: "aggregate" },
    { path: "artifacts/run/host.log", bytes: 40, hashMode: "head-tail-size" },
  ]);

  assert.deepEqual(result.byClass, {
    evidence: { files: 1, bytes: 40 },
    generated: { files: 1, bytes: 30 },
    source: { files: 1, bytes: 10 },
    test: { files: 1, bytes: 20 },
  });
  assert.deepEqual(result.hashModes, { aggregate: 1, full: 2, "head-tail-size": 1 });
  assert.equal(result.files, 4);
  assert.equal(result.bytes, 100);
});
