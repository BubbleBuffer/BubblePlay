import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

test("service validation requires an explicit URL only for live validation", {
  skip: process.platform !== "win32",
}, () => {
  const script = fileURLToPath(new URL("../../host/scripts/validate-service-update.ps1", import.meta.url));
  // Windows PowerShell startup can exceed 10s on a busy development machine.
  // Keep the guard assertions intact while allowing a bounded launch window.
  const options = { encoding: "utf8", env: { ...process.env, BUBBLEPLAY_URL: "" }, timeout: 60_000 };
  const missing = spawnSync("powershell.exe", ["-NoProfile", "-File", script], options);
  assert.ifError(missing.error);
  assert.equal(missing.status, 1, missing.stderr);
  assert.match(missing.stderr, /pass -Url or set BUBBLEPLAY_URL/);
  for (const [args, env] of [
    [["-Url", "https://relay.example.com/"], options.env],
    [[], { ...options.env, BUBBLEPLAY_URL: "http://127.0.0.1:8787/" }],
  ]) {
    const configured = spawnSync("powershell.exe", ["-NoProfile", "-File", script, ...args], { ...options, env });
    assert.ifError(configured.error);
    assert.equal(configured.status, 1, configured.stderr);
    assert.match(configured.stderr, /ManifestPath is required/);
    assert.doesNotMatch(configured.stderr, /relay URL is required/);
  }
});
