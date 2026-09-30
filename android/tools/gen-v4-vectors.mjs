// Regenerates app/src/test/resources/v4-client-vectors.json from the browser
// receiver's own implementation (worker/public/media-v4.js), so the Kotlin
// client messages are checked byte for byte against the JS ones.
//
//   node android/tools/gen-v4-vectors.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  ackMessageV4, keyframeMessageV4, nextBackoffMsV4, V4ReceiveGate,
} from "../../worker/public/media-v4.js";

const here = dirname(fileURLToPath(import.meta.url));
const out = join(here, "..", "app", "src", "test", "resources", "v4-client-vectors.json");

const ackInputs = [
  { generation: 7, seq: 42, recvTsMs: 1234.5, decodedSeq: 41 },
  { generation: 7, seq: 42, recvTsMs: 1234.5 },
  { generation: 0, seq: 0, recvTsMs: 0 },
  { generation: 1, seq: 2, recvTsMs: -3.2 },
  { generation: 4294967295, seq: 4294967295, recvTsMs: 99.94, decodedSeq: 4294967295 },
  { generation: 3, seq: 9, recvTsMs: 12.35 },
  { generation: 3, seq: 9, recvTsMs: 12.349999 },
  { generation: 3, seq: 9, recvTsMs: 0.04 },
  { generation: 3, seq: 9, recvTsMs: 0.05 },
  { generation: 3, seq: 9, recvTsMs: 1e7 + 0.26 },
  { generation: 3, seq: 9, recvTsMs: 123456789.123 },
  { generation: 3, seq: 9, recvTsMs: 5, decodedSeq: 0 },
];
const acks = ackInputs.map(input => ({ ...input, text: ackMessageV4(input) }));

const reasons = [
  "sequence-gap", "", null, "decoder error: bad NAL", "reconnect", "a".repeat(80), "héllo wörld",
  "generation-change", "client/start", "x".repeat(63) + "é",
];
const keyframes = reasons.map(reason => ({ reason, text: keyframeMessageV4(reason) }));

const backoffInputs = [0, -1, 100, 249, 250, 400, 2500, 4000, 5000, 9000];
const backoff = backoffInputs.map(previous => ({ previous, next: nextBackoffMsV4(previous) }));

// Receive-gate script: frames in, verdicts out.
const script = [
  { key: false, generation: 1, seq: 0 },
  { key: true, generation: 1, seq: 10 },
  { key: false, generation: 1, seq: 11 },
  { key: false, generation: 1, seq: 13 },
  { key: false, generation: 1, seq: 14 },
  { key: true, generation: 1, seq: 15 },
  { key: false, generation: 2, seq: 16 },
  { key: true, generation: 2, seq: 4294967295 },
  { key: false, generation: 2, seq: 0 },
  { invalidate: "decoder-error" },
  { key: false, generation: 2, seq: 1 },
  { key: true, generation: 2, seq: 2 },
  { reset: "reconnect" },
  { key: false, generation: 2, seq: 3 },
  { key: true, generation: 3, seq: 0 },
  { key: false, generation: 3, seq: 1 },
];
const gate = new V4ReceiveGate();
const gateSteps = script.map(step => {
  if (step.invalidate) { gate.invalidate(step.invalidate); return step; }
  if (step.reset) { gate.reset(step.reset); return step; }
  return { ...step, verdict: gate.accept(step) };
});

const vectors = {
  generatedBy: "android/tools/gen-v4-vectors.mjs from worker/public/media-v4.js",
  ping: JSON.stringify({ type: "ping" }),
  acks,
  keyframes,
  backoff,
  gate: gateSteps,
};
writeFileSync(out, JSON.stringify(vectors, null, 2) + "\n");
console.log(`wrote ${out}`);
