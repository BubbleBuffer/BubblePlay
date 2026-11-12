import test from "node:test";
import assert from "node:assert/strict";
import { createWssAudioSession, WSS_AUDIO_SINK, AUDIO_PACKET_REJECTED } from "../public/wss-audio.js";

function harness(results) {
  const calls = [];
  const players = [];
  const sinks = new Map();
  const soundController = {
    attachSink: (name, sink) => { calls.push(["attach", name]); sinks.set(name, sink); },
    detachSink: name => { calls.push(["detach", name]); sinks.delete(name); },
  };
  const session = createWssAudioSession({
    soundController, onRejected: () => calls.push(["rejected"]), log: () => {},
    createPlayer: () => {
      const player = { pushes: 0, stopped: 0,
        push: () => results[player.pushes++ % results.length],
        stop: () => { player.stopped++; }, resume: () => {}, setMuted: () => {},
        stats: () => ({ packets: 1 }) };
      players.push(player);
      return player;
    },
  });
  return { session, calls, players, sinks };
}

test("sink attaches on the first accepted packet only; rejected packets are reported", () => {
  const h = harness([AUDIO_PACKET_REJECTED, "ok", "ok"]);
  h.session.push(new Uint8Array());
  assert.deepEqual(h.calls, [["rejected"]]);
  h.session.push(new Uint8Array());
  h.session.push(new Uint8Array());
  assert.deepEqual(h.calls, [["rejected"], ["attach", WSS_AUDIO_SINK]]);
});

test("stop detaches the sink, stops the player and the next packet starts a fresh one", () => {
  const h = harness(["ok"]);
  h.session.push(new Uint8Array());
  h.session.stop();
  assert.deepEqual(h.calls, [["attach", WSS_AUDIO_SINK], ["detach", WSS_AUDIO_SINK]]);
  assert.equal(h.players[0].stopped, 1);
  h.session.push(new Uint8Array());
  assert.equal(h.players.length, 2);
});
