import test from "node:test";
import assert from "node:assert/strict";
import { createMediaAckScheduler } from "../public/media-ack.js";

const state = () => ({ protocol: 2,
  reassembler: { highestSequence: [7, 7, 7, 7], highestReconstructed: 7,
    stats: { recoveredFrames: 1, expiredFrames: 0 } },
  presented: 7, dependencyLosses: 0, deadline: { deadlineMs: 120, p95Ms: 8 },
  recoveryState: "healthy", decoderDrops: 0 });

test("v2 ACK does not access v3 completion telemetry", () => {
  let sent;
  const ack = createMediaAckScheduler({ snapshot: state, send: message => { sent = message; return true; } });
  ack.request(3, true);
  assert.deepEqual(sent, { type: "media-ack-v2", generation: 3, lanes: [7, 7, 7, 7],
    reconstructed: 7, presented: 7, recoveredFrames: 1, expiredFrames: 0,
    dependencyLosses: 0, deadlineMs: 120, reassemblyP95Ms: 8, recoveryState: "healthy", decoderDrops: 0 });
});

test("v3 ACK records the exact message and rejected signaling admission", () => {
  const current = { ...state(), protocol: 3, epoch: "0123456789abcdef0123456789abcdef",
    topology: { laneCount: 4, dataShards: 3, parityShards: 1 } };
  current.reassembler.stats.completionMissingByLane = [0, 0, 0, 1];
  let observed, sent;
  const ack = createMediaAckScheduler({ snapshot: () => current,
    send: message => { sent = message; return false; },
    record: (message, immediate, accepted) => { observed = { message, immediate, accepted }; } });
  ack.request(2, true);
  assert.equal(observed.message, sent);
  assert.equal(observed.accepted, false);
  assert.equal(observed.immediate, true);
  assert.equal(sent.generation, 2);
  assert.equal(sent.protocol, 3);
  assert.equal(sent.epoch, current.epoch);
  assert.deepEqual(sent.topology, current.topology);
  assert.deepEqual(sent.completionMissingByLane, [0, 0, 0, 1]);
});

test("ACKs coalesce, immediate recovery cancels a pending ACK, and cleanup is repeatable", t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const sent = [];
  const ack = createMediaAckScheduler({ snapshot: state, send: message => sent.push(message) });
  ack.request(1); ack.request(1);
  t.mock.timers.tick(99);
  assert.equal(sent.length, 0);
  t.mock.timers.tick(1);
  assert.equal(sent.length, 1);
  ack.request(1); ack.request(2, true);
  t.mock.timers.tick(100);
  assert.deepEqual(sent.map(message => message.generation), [1, 2]);
  ack.request(2); ack.clear(); ack.clear();
  t.mock.timers.tick(100);
  assert.equal(sent.length, 2);
});
