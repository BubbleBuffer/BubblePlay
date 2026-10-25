import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { installReceiverAssetOverrides } from "../tools/lab/stream-lab-live.mjs";

test("local receiver overrides include the entrypoint and its module dependencies", async () => {
  let paused;
  const calls = [];
  const receiver = { on: (_, handler) => { paused = handler; },
    call: async (method, params) => { calls.push({ method, params }); } };
  await installReceiverAssetOverrides(receiver, { baseUrl: "https://lab.example/" });
  const html = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  const pending = [...html.matchAll(/<script[^>]+src="([^"]+\.js)"/g)].map(match => match[1]);
  const visited = new Set();
  while (pending.length) {
    const path = pending.pop();
    if (visited.has(path)) continue;
    visited.add(path);
    assert.ok(calls[0].params.patterns.some(pattern => pattern.urlPattern === `https://lab.example${path}*`), path);
    await paused({ requestId: path, resourceType: "Script", request: { url: `https://lab.example${path}?build=local` } });
    const response = calls.at(-1);
    assert.equal(response.method, "Fetch.fulfillRequest", path);
    const body = Buffer.from(response.params.body, "base64").toString();
    assert.equal(body, readFileSync(new URL(`../public${path}`, import.meta.url), "utf8"));
    pending.push(...[...body.matchAll(/(?:from\s+|import\()["']((?:\.{1,2})?\/[^"']+\.js)["']/g)]
      .map(match => new URL(match[1], `https://lab.example${path}`).pathname)
      // The lab overrides top-level public/*.js; vendored files (the KDF worker's argon2) are unchanged builds.
      .filter(dependency => !dependency.startsWith("/vendor/")));
  }
  assert.ok(visited.has("/dk-app.js"));
  assert.ok(visited.has("/receiver.js"));
  assert.ok(visited.has("/dk-relay.js"));
  assert.ok(visited.has("/media-v4.js"));
  await paused({ requestId: "external", resourceType: "Script", request: { url: "https://other.example/receiver.js" } });
  assert.equal(calls.at(-1).method, "Fetch.continueRequest");
});
