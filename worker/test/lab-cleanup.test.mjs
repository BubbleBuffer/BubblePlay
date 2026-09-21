import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";

test("a rejected signal cleanup exits once without an unhandled rejection", () => {
  // Isolate the process-level failure rather than suppressing unhandledRejection
  // in the test runner. hardExit lets Node flush the rejected promise naturally.
  const moduleUrl = new URL("../tools/stream-lab-live.mjs", import.meta.url).href;
  const child = spawnSync(process.execPath, ["--input-type=module", "-e", `
    import { EventEmitter } from 'node:events';
    import { installTerminationCleanup } from ${JSON.stringify(moduleUrl)};
    const target = new EventEmitter();
    const dispose = installTerminationCleanup(async () => { throw new Error('cleanup failed'); }, {
      target, hardExit: code => { console.log('exit=' + code); dispose(); }
    });
    target.emit('SIGINT');
  `], { encoding: "utf8" });
  assert.equal(child.status, 0, child.stderr);
  assert.equal(child.stdout.trim(), "exit=130");
});
