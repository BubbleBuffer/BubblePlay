import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

// The relay serves the viewer under a CSP without script-src 'unsafe-inline'
// (relay-rs/core/src/headers.rs). A page script or handler written inline would be blocked.
test("index.html has no inline script and no inline event handler", () => {
  const html = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  for (const [, attributes, body] of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
    assert.match(attributes, /\bsrc=/, "every <script> loads a file");
    assert.equal(body.trim(), "");
  }
  assert.doesNotMatch(html, /<[a-z][^>]*\son[a-z]+\s*=/i, "no on* attribute");
  assert.doesNotMatch(html, /javascript:/i);
});
