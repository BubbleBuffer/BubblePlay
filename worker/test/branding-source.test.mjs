import test from "node:test";
import assert from "node:assert/strict";
import { DEFAULT_SOURCE_URL, sourceURL } from "../public/settings.js";

test("About source URL accepts operator sources and refuses executable schemes", async () => {
  const modified = "https://example.org/modified-source";
  for (const normalize of [sourceURL]) {
    assert.equal(normalize(modified), modified);
    for (const value of [undefined, "", "javascript:alert(1)", "file:///tmp/source", "https://user:password@example.org/"])
      assert.equal(normalize(value), DEFAULT_SOURCE_URL);
  }
});
