import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("../public/stream-lab-source.html", import.meta.url), "utf8");

test("stream-lab source publishes the deterministic controller API", () => {
  assert.match(source, /window\.__bubbleplaySource\s*=/);
  assert.match(source, /setScene/);
  assert.match(source, /setSeed/);
  assert.match(source, /pauseAt/);
  assert.match(source, /triggerAction/);
  assert.match(source, /metadata/);
  assert.match(source, /requestAnimationFrame/);
});

test("every temporal source frame carries timestamp and frame identity", () => {
  assert.match(source, /data-frame-id/);
  assert.match(source, /drawBarcode/);
  assert.match(source, /epochMs/);
  assert.match(source, /frameId/);
  assert.match(source, /crc8/);
});

test("timestamp identity uses one constant-area marker per bit without painting epoch digits", () => {
  assert.match(source, /const x0 = 64, y0 = 64, size = 24, markSize = 8/);
  assert.match(source, /const value = \(bytes\[index >> 3\] >> \(7 - \(index & 7\)\)\) & 1/);
  assert.match(source, /context\.fillRect\(\s*x0 \+ \(index % 32\) \* size \+ \(value \? markSize : 0\),/);
  assert.match(source, /identity marker/);
  assert.doesNotMatch(source, /fillText\(`\$\{scene\} · seed \$\{seed\} · frame \$\{currentFrameId\} · \$\{currentEpochMs\}`/);
});

test("source includes the complete named scene corpus", () => {
  for (const id of ["ui-text", "thin-lines", "gradients", "scrolling", "motion-60", "localized-change"])
    assert.match(source, new RegExp(`['\"]${id}['\"]`));
});
