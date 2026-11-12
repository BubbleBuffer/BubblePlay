import assert from "node:assert/strict";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { compare, countLines, measureSuppressions, parseClippy } from "../lint/gates.mjs";

test("ratchet: growth and unlowered shrink both fail, equal passes", () => {
  const baseline = { "a.rs": 600, "b.rs": 700, "c.rs": 800 };
  const { grew, stale } = compare({ "a.rs": 601, "b.rs": 650, "c.rs": 800, "d.rs": 501 }, baseline);
  assert.deepEqual(grew.map(item => item.key).sort(), ["a.rs", "d.rs"]);
  assert.deepEqual(stale.map(item => item.key), ["b.rs"]);
  assert.deepEqual(compare({ "c.rs": 800 }, { "c.rs": 800 }), { grew: [], stale: [] });
  // A file that dropped under the limit is absent from `current` and must leave the baseline.
  assert.deepEqual(compare({}, { "a.rs": 600 }).stale.map(item => item.key), ["a.rs"]);
});

test("line counting matches wc -l for files with and without a final newline", () => {
  assert.equal(countLines("a\nb\n"), 2);
  assert.equal(countLines("a\nb"), 2);
  assert.equal(countLines(""), 0);
});

test("suppressions are counted per component and kind", () => {
  const files = { "host/src/a.rs": "#[allow(dead_code)]\n#![allow(x)]", "worker/public/a.js": "try {} catch (_) {}\n// eslint-disable-line" };
  const counts = measureSuppressions(Object.keys(files), path => files[path]);
  assert.deepEqual(counts, {
    "host\u0000#[allow(": 2, "worker\u0000empty catch": 1, "worker\u0000eslint-disable": 1,
  });
});

test("clippy: only ratcheted lints inside the project, each location once", () => {
  const diag = (code, file) => JSON.stringify({ reason: "compiler-message", message: { code: { code },
    spans: [{ is_primary: true, file_name: file, line_start: 3, column_start: 5 }] } });
  const out = [diag("clippy::unwrap_used", "src/a.rs"), diag("clippy::unwrap_used", "src/a.rs"),
    diag("clippy::needless_return", "src/a.rs"), diag("clippy::expect_used", "../crates/x/src/b.rs"), "not json"].join("\n");
  const project = { dir: "host", skip: [] };
  const counts = parseClippy(out, project, fileURLToPath(new URL("../../host", import.meta.url)));
  assert.deepEqual(counts, { "host/src/a.rs\u0000clippy::unwrap_used": 1 });
});
