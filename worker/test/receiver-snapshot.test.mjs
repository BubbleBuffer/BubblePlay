import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replaceAll("\r\n", "\n");

// The snapshot the receiver hands the peer's startup fallback: receiver.js cannot be imported
// (it needs the page), so its startupSnapshot callback is evaluated on its own.
function startupSnapshotFor(fields) {
  const start = source.indexOf("startupSnapshot: generation =>");
  const end = source.indexOf("}),", start) + "})".length;
  const snapshot = vm.runInNewContext(`(${source.slice(start, end).replace("startupSnapshot:", "")})`, fields);
  return generation => ({ ...snapshot(generation) });
}

test("the startup snapshot counts frames received and native frames of the asked peer generation only", () => {
  const nativePresentation = { generation: 3, framesPresented: 7 };
  const snapshot = startupSnapshotFor({ framesReceived: 5, nativePresentation });
  assert.deepEqual(snapshot(3), { framesReceived: 5, nativeFramesPresented: 7 });
  assert.deepEqual(snapshot(4), { framesReceived: 5, nativeFramesPresented: 0 }, "a replaced peer does not inherit the old count");
  assert.deepEqual(startupSnapshotFor({ framesReceived: 0, nativePresentation: null })(3), { framesReceived: 0, nativeFramesPresented: 0 });
});
