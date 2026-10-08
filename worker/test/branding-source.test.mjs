import test from "node:test";
import assert from "node:assert/strict";
import { ApprovalGate, sourceURL as relaySourceURL } from "../src/gate.js";
import { DEFAULT_SOURCE_URL, loadSourceLink, sourceURL } from "../public/settings.js";

test("About source URL accepts operator sources and refuses executable schemes", async () => {
  const modified = "https://example.org/modified-source";
  for (const normalize of [sourceURL, relaySourceURL]) {
    assert.equal(normalize(modified), modified);
    for (const value of [undefined, "", "javascript:alert(1)", "file:///tmp/source", "https://user:password@example.org/"])
      assert.equal(normalize(value), DEFAULT_SOURCE_URL);
  }
  const link = {};
  await loadSourceLink(link, async (path, options) => {
    assert.equal(path, "/__auth/methods");
    assert.equal(options.cache, "no-store");
    return Response.json({ sourceURL: modified });
  });
  assert.equal(link.href, modified);
  await loadSourceLink(link, async () => { throw new Error("offline"); });
  assert.equal(link.href, DEFAULT_SOURCE_URL);
});

test("only the two favicon files are public before either approval mode", async () => {
  for (const methods of ["host", "ntfy"]) {
    const gate = new ApprovalGate({ storage: { async get() {} } }, {
      APPROVAL_METHODS: methods,
      NTFY_URL: "https://ntfy.example.org",
      NTFY_TOPIC: "synthetic-test-topic",
      NTFY_TOKEN: "synthetic-test-token",
      SOURCE_URL: "https://example.org/modified-source",
      ASSETS: { async fetch(request) { return new Response(new URL(request.url).pathname); } },
    });
    const get = path => gate.fetch(new Request(`https://example.org${path}`));
    for (const path of ["/favicon.svg", "/favicon-32.png"]) {
      const response = await get(path);
      assert.equal(response.status, 200);
      assert.equal(await response.text(), path);
    }
    for (const path of ["/favicon.ico", "/favicon-other.png", "/receiver.js", "/login.html"])
      assert.equal((await get(path)).status, 404);
    const response = await get("/__auth/methods");
    assert.equal(response.headers.get("Cache-Control"), "no-store");
    assert.equal((await response.json()).sourceURL, "https://example.org/modified-source");
  }
});
