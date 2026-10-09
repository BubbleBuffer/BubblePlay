import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { requireLabUrl } from "../tools/lab/relay-url.mjs";
import { LiveStreamLabSession, installReceiverAssetOverrides } from "../tools/lab/stream-lab-live.mjs";

test("live sessions require a URL before consulting process or service configuration", async () => {
  const contacts = [];
  assert.throws(() => new LiveStreamLabSession({
    env: {},
    get chrome() { contacts.push("chrome"); throw new Error("unexpected process lookup"); },
    get store() { contacts.push("store"); throw new Error("unexpected artifact access"); },
  }), /pass --url or set BUBBLEPLAY_URL/);
  await assert.rejects(installReceiverAssetOverrides({
    on() { contacts.push("listener"); }, call() { contacts.push("CDP"); },
  }, { env: {} }), /pass --url or set BUBBLEPLAY_URL/);
  assert.deepEqual(contacts, []);

  const options = { runId: "url-test", store: {}, chrome: process.execPath, env: {} };
  assert.equal(new LiveStreamLabSession({ ...options, baseUrl: "https://relay.example.com/" }).baseUrl,
    "https://relay.example.com/");
  assert.equal(new LiveStreamLabSession({ ...options, env: { BUBBLEPLAY_URL: "http://127.0.0.1:8787/" } }).baseUrl,
    "http://127.0.0.1:8787/");
  for (const value of [undefined, "", "   ", "relay.example.com", "file:///tmp/relay"]) {
    assert.throws(() => requireLabUrl(value), /relay URL/);
  }
});

test("live CLIs reject a missing relay URL before files, children, services or network access", () => {
  for (const [name, args] of [["latency-live.mjs", []], ["stream-lab.mjs", ["smoke"]]]) {
    const entry = new URL(`../tools/lab/${name}`, import.meta.url);
    const child = spawnSync(process.execPath, ["--input-type=module", "-e", `
      import fs from 'node:fs';
      import cp from 'node:child_process';
      import { syncBuiltinESMExports } from 'node:module';
      const forbidden = () => { throw new Error('UNEXPECTED_SIDE_EFFECT'); };
      const openSync = fs.openSync;
      fs.openSync = (file, flags, ...rest) => {
        // Node must read modules to reach the CLI's configuration check.
        if (flags === 'r' && /\\.(?:mjs|js|json)$/.test(String(file))) return openSync(file, flags, ...rest);
        return forbidden();
      };
      for (const key of ['existsSync','mkdirSync','mkdtempSync','writeFileSync']) fs[key] = forbidden;
      cp.spawn = forbidden;
      cp.spawnSync = forbidden;
      globalThis.fetch = forbidden;
      globalThis.WebSocket = forbidden;
      syncBuiltinESMExports();
      process.argv = [process.execPath, ${JSON.stringify(fileURLToPath(entry))}, ...${JSON.stringify(args)}];
      await import(${JSON.stringify(entry.href)});
    `], { encoding: "utf8", env: { ...process.env, BUBBLEPLAY_URL: "", BUBBLEPLAY_TOKEN: "" }, timeout: 60_000 });
    assert.ifError(child.error);
    assert.equal(child.status, 1, child.stderr);
    assert.match(child.stderr, /pass --url or set BUBBLEPLAY_URL/, name);
    assert.doesNotMatch(child.stderr, /UNEXPECTED_SIDE_EFFECT/, name);
  }
});

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
