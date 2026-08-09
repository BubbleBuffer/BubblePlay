import test from "node:test";
import assert from "node:assert/strict";
import {
  createEncodedWorkerState,
  createLatencyGuard,
  annexBHasIdr,
  decodeTimestampPatternBits,
  normalizeEncodedTimestamp,
  createLatestRenderSlot,
  createWorkerTimeline,
  summarizeRtcStats,
} from "../public/latency.js";

test("limits worker-to-main and decode work to two frames", () => {
  const guard = createLatencyGuard({ now: () => 0 });
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 1_000 }), true);
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 17_000 }), true);
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 34_000 }), false);
  assert.equal(guard.admitDecode(), true);
  assert.equal(guard.admitDecode(), true);
  assert.equal(guard.admitDecode(), false);
});

test("late frames reset after three consecutive 100ms lag violations", () => {
  let now = 0;
  let resets = 0;
  const guard = createLatencyGuard({ now: () => now, onReset: () => resets++, startupGraceMs: 0 });
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 1_000_000 }), true);
  guard.releaseWorkerFrame();
  now = 150;
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 1_016_000 }), true);
  guard.releaseWorkerFrame();
  now = 200;
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 1_032_000 }), true);
  guard.releaseWorkerFrame();
  now = 250;
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 1_048_000 }), false);
  assert.equal(resets, 1);
  assert.equal(guard.generation, 1);
});

test("timestamp discontinuity creates a new generation", () => {
  let resets = 0;
  const guard = createLatencyGuard({ now: () => 0, onReset: () => resets++ });
  guard.acceptWorkerFrame({ generation: 0, timestamp: 1_000_000 });
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 9_000_000 }), false);
  assert.equal(resets, 1);
  assert.equal(guard.generation, 1);
});

test("worker ACKs are generation and sequence aware across a resync", () => {
  const state = createEncodedWorkerState();
  const old = state.reserve("key");
  state.resync("test");
  const current = state.reserve("key");
  assert.equal(state.ack(old), false);
  assert.equal(state.pending, 1);
  assert.equal(state.ack(current), true);
  assert.equal(state.pending, 0);
});

test("worker snapshots reservations before a reset and only posts current generation", () => {
  const state = createEncodedWorkerState();
  const delayed = state.reserve("key");
  state.resync("during-delay");
  assert.equal(state.canPost(delayed), false);
  assert.equal(state.reserve("delta"), null, "deltas wait for the recovery keyframe");
  assert.ok(state.reserve("key"));
});

test("echoed resync generation is idempotent instead of advancing forever", () => {
  let notifications = 0;
  const state = createEncodedWorkerState({ onResync: () => notifications++ });
  state.resync("worker");
  const generation = state.generation;
  state.resync("main-echo", generation);
  assert.equal(state.generation, generation);
  assert.equal(notifications, 1);
});

test("deltas discarded while waiting for a keyframe do not churn generations", () => {
  let notifications = 0;
  const state = createEncodedWorkerState({ onResync: () => notifications++ });
  state.resync("overflow");
  const generation = state.generation;
  assert.equal(state.reserve("delta"), null);
  assert.equal(state.reserve("delta"), null);
  assert.equal(state.generation, generation);
  assert.equal(notifications, 1);
  assert.ok(state.reserve("key"));
});

test("presentation age is derived from the media timeline, not decoder callback time", () => {
  let now = 1_000;
  const guard = createLatencyGuard({ now: () => now, startupGraceMs: 0 });
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 5_000_000 }), true);
  now = 1_060;
  assert.equal(guard.presentationAgeMs(5_016_000, now), 44);
  now = 1_200;
  assert.equal(guard.presentationAgeMs(5_032_000, now), 168);
});

test("startup grace and decoded-frame reanchor avoid a false warm-up reset", () => {
  let now = 0;
  let resets = 0;
  const guard = createLatencyGuard({ now: () => now, onReset: () => resets++ });
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 1_000_000 }), true);
  guard.releaseWorkerFrame();
  now = 400;
  assert.equal(guard.acceptWorkerFrame({ generation: 0, timestamp: 1_016_000 }), true);
  guard.releaseWorkerFrame();
  guard.reanchor(1_016_000, now);
  assert.equal(guard.presentationAgeMs(1_032_000, 420), 4);
  assert.equal(resets, 0);
});

test("timestamp barcode decoder validates preamble and checksum", () => {
  const timestamp = 0x12345678;
  const sequence = 0x9abc;
  const bytes = [0xa5, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
  let crc = 0;
  for (const byte of bytes.slice(1)) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc & 0x80) ? ((crc << 1) ^ 0x07) & 0xff : (crc << 1) & 0xff;
  }
  const bits = [...bytes, crc].flatMap((byte) => Array.from({ length: 8 }, (_, bit) => (byte >> (7 - bit)) & 1));
  assert.deepEqual(decodeTimestampPatternBits(bits), { timestamp, sequence });
  bits[63] ^= 1;
  assert.equal(decodeTimestampPatternBits(bits), null);
});

test("Annex-B IDR detection does not depend on the browser frame label", () => {
  assert.equal(annexBHasIdr(Uint8Array.from([0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x65, 9])), true);
  assert.equal(annexBHasIdr(Uint8Array.from([0, 0, 1, 0x41, 9, 8, 7])), false);
});

test("encoded timestamps prefer microsecond PTS and normalize RTP 90 kHz fallback", () => {
  assert.equal(normalizeEncodedTimestamp(90_000, { timestamp: 1_234_567, rtpTimestamp: 90_000 }), 1_234_567);
  assert.equal(normalizeEncodedTimestamp(90_000, { rtpTimestamp: 90_000 }), 1_000_000);
  assert.equal(normalizeEncodedTimestamp(1_234_567, {}), 1_234_567);
});

test("production worker timeline resets only after three sustained late encoded frames", () => {
  let now = 0;
  const timeline = createWorkerTimeline({ now: () => now, startupGraceMs: 0 });
  assert.equal(timeline.observe(1_000_000).ok, true);
  now = 150; assert.equal(timeline.observe(1_016_000).ok, true);
  now = 200; assert.equal(timeline.observe(1_032_000).ok, true);
  now = 250; assert.deepEqual(timeline.observe(1_048_000), { ok: false, reason: "three-late-frames" });
  timeline.reset();
  assert.equal(timeline.observe(2_000_000).ok, true);
  assert.deepEqual(timeline.observe(5_000_001), { ok: false, reason: "timestamp-discontinuity" });
});

test("production worker timeline accepts a matching multi-second static-desktop gap", () => {
  let now = 0;
  const timeline = createWorkerTimeline({ now: () => now, startupGraceMs: 0 });
  assert.equal(timeline.observe(1_000_000).ok, true);
  now = 3_000;
  assert.deepEqual(timeline.observe(4_000_000), { ok: true, due: 3_000 });
});

test("latest render slot replaces frames without postponing the armed deadline", () => {
  const slot = createLatestRenderSlot();
  assert.deepEqual(slot.offer("first"), { arm: true, replaced: null });
  assert.deepEqual(slot.offer("second"), { arm: false, replaced: "first" });
  assert.deepEqual(slot.offer("latest"), { arm: false, replaced: "second" });
  assert.equal(slot.take(), "latest");
  assert.deepEqual(slot.offer("next"), { arm: true, replaced: null });
});

test("WebRTC profiling preserves decoder, recovery, codec, and candidate counters", () => {
  const report = new Map([
    ["in", { id: "in", type: "inbound-rtp", kind: "video", timestamp: 1234,
      codecId: "codec", packetsReceived: 900, packetsLost: 7, bytesReceived: 1_500_000,
      jitter: 0.012, jitterBufferDelay: 4.5, jitterBufferEmittedCount: 300,
      framesReceived: 301, framesDecoded: 299, framesDropped: 2, keyFramesDecoded: 4,
      freezeCount: 3, totalFreezesDuration: 1.25, totalDecodeTime: 2.5,
      nackCount: 8, pliCount: 2, firCount: 1, retransmittedPacketsReceived: 6,
      decoderImplementation: "ExternalDecoder", powerEfficientDecoder: true }],
    ["pair", { id: "pair", type: "candidate-pair", state: "succeeded", nominated: true,
      localCandidateId: "local", remoteCandidateId: "remote", currentRoundTripTime: 0.087,
      availableIncomingBitrate: 6_000_000 }],
    ["local", { id: "local", type: "local-candidate", candidateType: "srflx", protocol: "udp", networkType: "cellular" }],
    ["remote", { id: "remote", type: "remote-candidate", candidateType: "host", protocol: "udp" }],
    ["codec", { id: "codec", type: "codec", mimeType: "video/H264", clockRate: 90_000, sdpFmtpLine: "profile-level-id=64002a" }],
  ]);
  const profile = summarizeRtcStats(report);
  assert.equal(profile.inbound.packetsLost, 7);
  assert.equal(profile.inbound.jitterMs, 12);
  assert.equal(profile.inbound.freezeCount, 3);
  assert.equal(profile.inbound.decoderImplementation, "ExternalDecoder");
  assert.equal(profile.candidatePair.currentRoundTripTimeMs, 87);
  assert.equal(profile.candidatePair.local.candidateType, "srflx");
  assert.equal(profile.codec.mimeType, "video/H264");
});
